data "aws_partition" "current" {}

data "aws_ami" "ubuntu" {
  filter {
    name   = "image-id"
    values = [var.ami_id]
  }
}

locals {
  prefix = var.tag_prefix

  # the tags the adapter writes on an instance, its root volume and its fleet (and, on the old
  # RunInstances path, its Spot request), and nothing else
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
  fleet_arn              = "${local.ec2_account_arn_prefix}:fleet/*"
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

# what every launch shares. A fleet cannot override the security group, so it lives here, and
# no network interface block, so the subnets' automatic public IPv4 applies. Each launch pins a
# version number, so a change here takes effect only once the AWS classes in MACHINES_CONFIG
# name the new version. The image, subnet, root volume, IMDSv2 and user data come with each
# launch as overrides
resource "aws_launch_template" "machines" {
  name        = "cloudlease-machines"
  description = "cloudlease machines: outbound-only Spot instances"

  image_id                             = var.ami_id
  instance_initiated_shutdown_behavior = "terminate"
  vpc_security_group_ids               = [aws_security_group.machines.id]

  # each launch asks for IMDSv2 too; IAM has no IMDS condition key for CreateFleet, so the
  # template holds it as well
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }

  tags = merge(local.common_tags, { Name = "cloudlease-machines" })
}

# the boundary comes from infra/aws-operator; the operator may change this user only while it is
# attached, so a plan that would drop it fails with AccessDenied
resource "aws_iam_user" "worker" {
  name                 = var.iam_user_name
  path                 = var.iam_path
  permissions_boundary = var.permissions_boundary_arn
  force_destroy        = false

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

  # launching from a launch template needs RunInstances on the template, a fleet's launch too
  statement {
    sid     = "UseFixedImageTemplateAndSubnets"
    effect  = "Allow"
    actions = ["ec2:RunInstances"]
    resources = concat(
      [local.image_arn, local.snapshot_arn, aws_launch_template.machines.arn],
      local.subnet_arns,
    )

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

# What EC2 checks on an instant CreateFleet, probed as the Worker on 2026-11-25 with DryRun and
# Deny-if-absent statements (see the README): the instance, the fleet and the root volume with
# ec2:Region and the request tags; the launch template, the image and each subnet with their
# usual keys. The instance check carries no ec2:AvailabilityZone, ec2:InstanceType or
# ec2:Tenancy and the volume check no ec2:VolumeSize, although the Service Authorization
# Reference lists them, so a condition on any of them can never match. The security group,
# network interface, snapshot and Spot request are not checked at all. The zone rests on the
# pinned subnets, IMDSv2 and the security group on the launch template, and Spot, the instance
# type and the root volume size on the request the adapter sends. The RunInstances statements
# above stay in case a fleet's launch is also authorized against the caller
data "aws_iam_policy_document" "fleet" {
  statement {
    sid       = "CreateFleetOfTaggedInstance"
    effect    = "Allow"
    actions   = ["ec2:CreateFleet"]
    resources = [local.instance_arn]

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

  # the fleet and the root volume carry the instance's tags
  statement {
    sid       = "CreateTaggedFleetAndRootVolume"
    effect    = "Allow"
    actions   = ["ec2:CreateFleet"]
    resources = [local.fleet_arn, local.volume_arn]

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

  # each subnet sits in one allowed zone of this VPC, and the template's security group keeps a
  # launch inside this VPC
  statement {
    sid       = "FleetFromPinnedTemplateImageAndSubnets"
    effect    = "Allow"
    actions   = ["ec2:CreateFleet"]
    resources = concat([aws_launch_template.machines.arn, local.image_arn], local.subnet_arns)

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = [var.aws_region]
    }
  }

  statement {
    sid       = "TagOnlyOnFleetLaunch"
    effect    = "Allow"
    actions   = ["ec2:CreateTags"]
    resources = [local.fleet_arn, local.instance_arn, local.volume_arn]

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["CreateFleet"]
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
    fleet     = data.aws_iam_policy_document.fleet.json
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
