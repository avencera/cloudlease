data "aws_partition" "current" {}

data "aws_ami" "ubuntu" {
  filter {
    name   = "image-id"
    values = [var.ami_id]
  }
}

locals {
  prefix = var.tag_prefix

  # the tags the adapter writes on an instance and its Spot request, and nothing else
  managed_tag_keys = [
    "Name",
    "${local.prefix}-gen",
    "${local.prefix}-kind",
    "${local.prefix}-managed",
    "${local.prefix}-owner",
    "${local.prefix}-until",
  ]

  common_tags = {
    ManagedBy = "Terraform"
    Service   = "cloudlease-machines"
  }

  ec2_account_arn_prefix = "arn:${data.aws_partition.current.partition}:ec2:${var.aws_region}:${var.aws_account_id}"
  image_arn              = "arn:${data.aws_partition.current.partition}:ec2:${var.aws_region}::image/${var.ami_id}"
  instance_arn           = "${local.ec2_account_arn_prefix}:instance/*"
  network_interface_arn  = "${local.ec2_account_arn_prefix}:network-interface/*"
  snapshot_arn           = "arn:${data.aws_partition.current.partition}:ec2:${var.aws_region}:${data.aws_ami.ubuntu.owner_id}:snapshot/${data.aws_ami.ubuntu.root_snapshot_id}"
  spot_request_arn       = "${local.ec2_account_arn_prefix}:spot-instances-request/*"
  volume_arn             = "${local.ec2_account_arn_prefix}:volume/*"
  subnet_arns            = [for subnet in aws_subnet.machines : subnet.arn]
}

# a VPC of its own, so the Worker's network permissions name nothing another app uses
resource "aws_vpc" "machines" {
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = merge(local.common_tags, { Name = "cloudlease-machines" })
}

resource "aws_subnet" "machines" {
  for_each = { for index, zone in var.availability_zones : zone => index }

  availability_zone       = each.key
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, each.value)
  map_public_ip_on_launch = true
  vpc_id                  = aws_vpc.machines.id

  tags = merge(local.common_tags, { Name = "cloudlease-machines-${each.key}" })
}

resource "aws_internet_gateway" "machines" {
  vpc_id = aws_vpc.machines.id

  tags = merge(local.common_tags, { Name = "cloudlease-machines" })
}

resource "aws_route_table" "machines" {
  vpc_id = aws_vpc.machines.id

  tags = merge(local.common_tags, { Name = "cloudlease-machines-public" })
}

resource "aws_route" "public_internet" {
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = aws_internet_gateway.machines.id
  route_table_id         = aws_route_table.machines.id
}

resource "aws_route_table_association" "machines" {
  for_each = aws_subnet.machines

  route_table_id = aws_route_table.machines.id
  subnet_id      = each.value.id
}

# machines reach their coordinator outbound over iroh, so nothing may connect in; Terraform
# owns the empty ingress and the Worker cannot change it
resource "aws_security_group" "machines" {
  name        = "cloudlease-machines"
  description = "cloudlease machines: outbound only"
  vpc_id      = aws_vpc.machines.id

  egress {
    from_port        = 0
    to_port          = 0
    protocol         = "-1"
    cidr_blocks      = ["0.0.0.0/0"]
    ipv6_cidr_blocks = ["::/0"]
  }

  tags = merge(local.common_tags, { Name = "cloudlease-machines" })
}

resource "aws_iam_user" "worker" {
  name          = var.iam_user_name
  path          = var.iam_path
  force_destroy = false

  tags = local.common_tags
}

data "aws_iam_policy_document" "inventory" {
  statement {
    sid    = "ReadInstancesAndSpotRequests"
    effect = "Allow"
    actions = [
      "ec2:DescribeInstances",
      "ec2:DescribeSpotInstanceRequests",
    ]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }
  }
}

data "aws_iam_policy_document" "launch" {
  statement {
    sid       = "RunTaggedSpotInstance"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = [local.instance_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:AvailabilityZone"
      values   = var.availability_zones
    }

    condition {
      test     = "StringLike"
      variable = "ec2:InstanceType"
      values   = sort(tolist(var.allowed_instance_type_patterns))
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:InstanceMarketType"
      values   = ["spot"]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:MetadataHttpTokens"
      values   = ["required"]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:Tenancy"
      values   = ["default"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.prefix}-managed"
      values   = ["true"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.prefix}-kind"
      values   = ["instance"]
    }

    condition {
      test     = "Null"
      variable = "aws:RequestTag/${local.prefix}-owner"
      values   = ["false"]
    }

    condition {
      test     = "Null"
      variable = "aws:RequestTag/${local.prefix}-gen"
      values   = ["false"]
    }

    condition {
      test     = "Null"
      variable = "aws:RequestTag/${local.prefix}-until"
      values   = ["false"]
    }

    condition {
      test     = "Null"
      variable = "aws:RequestTag/Name"
      values   = ["false"]
    }

    condition {
      test     = "ForAllValues:StringEquals"
      variable = "aws:TagKeys"
      values   = local.managed_tag_keys
    }
  }

  statement {
    sid       = "UseFixedImageAndSubnets"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = concat([local.image_arn, local.snapshot_arn], local.subnet_arns)

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }
  }

  statement {
    sid       = "CreateNetworkInterfaceInOwnSubnets"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = [local.network_interface_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }

    condition {
      test     = "ArnEquals"
      variable = "ec2:Subnet"
      values   = local.subnet_arns
    }

    condition {
      test     = "ArnEquals"
      variable = "ec2:Vpc"
      values   = [aws_vpc.machines.arn]
    }

    condition {
      test     = "Bool"
      variable = "ec2:AssociatePublicIpAddress"
      values   = ["true"]
    }
  }

  statement {
    sid       = "UseOutboundOnlySecurityGroup"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = [aws_security_group.machines.arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }
  }

  statement {
    sid       = "CreateBoundedRootVolume"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = [local.volume_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }

    condition {
      test     = "NumericLessThanEquals"
      variable = "ec2:VolumeSize"
      values   = [tostring(var.max_root_volume_gib)]
    }
  }

  statement {
    sid       = "RunTaggedSpotRequest"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = [local.spot_request_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.prefix}-managed"
      values   = ["true"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.prefix}-kind"
      values   = ["instance"]
    }

    condition {
      test     = "ForAllValues:StringEquals"
      variable = "aws:TagKeys"
      values   = local.managed_tag_keys
    }
  }

  statement {
    sid       = "TagOnlyOnLaunch"
    effect    = "Allow"
    actions   = ["ec2:CreateTags"]
    resources = [local.instance_arn, local.spot_request_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["RunInstances"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.prefix}-managed"
      values   = ["true"]
    }

    condition {
      test     = "ForAllValues:StringEquals"
      variable = "aws:TagKeys"
      values   = local.managed_tag_keys
    }
  }
}

data "aws_iam_policy_document" "cleanup" {
  statement {
    sid       = "TerminateOnlyOwnInstances"
    effect    = "Allow"
    actions   = ["ec2:TerminateInstances"]
    resources = [local.instance_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/${local.prefix}-managed"
      values   = ["true"]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/${local.prefix}-kind"
      values   = ["instance"]
    }
  }

  statement {
    sid       = "CancelOnlyOwnSpotRequests"
    effect    = "Allow"
    actions   = ["ec2:CancelSpotInstanceRequests"]
    resources = [local.spot_request_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/${local.prefix}-managed"
      values   = ["true"]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/${local.prefix}-kind"
      values   = ["instance"]
    }
  }
}

locals {
  policy_documents = {
    cleanup   = data.aws_iam_policy_document.cleanup.json
    inventory = data.aws_iam_policy_document.inventory.json
    launch    = data.aws_iam_policy_document.launch.json
  }
}

resource "aws_iam_policy" "worker" {
  for_each = local.policy_documents

  name        = "${var.iam_user_name}-${var.aws_region}-${each.key}"
  path        = var.iam_path
  description = "cloudlease machines Worker EC2 Spot ${each.key} permissions in ${var.aws_region}"
  policy      = each.value

  tags = local.common_tags

  lifecycle {
    precondition {
      condition     = length(each.value) <= 6144
      error_message = "The generated IAM policy exceeds the AWS managed-policy size limit"
    }
  }
}

resource "aws_iam_user_policy_attachment" "worker" {
  for_each = aws_iam_policy.worker

  user       = aws_iam_user.worker.name
  policy_arn = each.value.arn
}
