# Permissions boundaries cap what the service users can ever do, whatever the operator writes
# into their policies. Each one allows the actions, resource types, regions and ownership tags
# its user's current policies use; the user's own policies keep the finer limits (AMI, subnets,
# instance types, volume size, tag sets). The operator holds no write on these policies, so
# only an administrator can widen a boundary

locals {
  ec2_arn_prefix = { for region in local.all_regions : region => "arn:${local.partition}:ec2:${region}:${var.aws_account_id}" }

  all_regions = distinct(concat(var.machines_regions, var.rembuild_regions))

  # rembuild still tags builders in two shapes until its legacy statements are removed:
  # rb-managed=rembuild with rb-kind=builder, and rb-managed=true with rb-kind=instance
  rembuild_managed_values = ["rembuild", "true"]
}

# the resources RunInstances also touches besides the instance and its Spot request; the users'
# own policies pin the image, subnets and security group
locals {
  launch_resource_arns = {
    for region in local.all_regions : region => [
      "arn:${local.partition}:ec2:${region}::image/*",
      # an AMI's root snapshot belongs to the AMI's owner, which for Canonical's images is
      # another account
      "arn:${local.partition}:ec2:${region}:*:snapshot/*",
      "${local.ec2_arn_prefix[region]}:network-interface/*",
      "${local.ec2_arn_prefix[region]}:security-group/*",
      "${local.ec2_arn_prefix[region]}:subnet/*",
      "${local.ec2_arn_prefix[region]}:volume/*",
    ]
  }
}

data "aws_iam_policy_document" "machines_boundary" {
  statement {
    sid       = "Inventory"
    actions   = ["ec2:DescribeInstances", "ec2:DescribeSpotInstanceRequests"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = var.machines_regions
    }
  }

  statement {
    sid     = "LaunchManagedInstance"
    actions = ["ec2:RunInstances"]
    resources = flatten([
      for region in var.machines_regions : [
        "${local.ec2_arn_prefix[region]}:instance/*",
        "${local.ec2_arn_prefix[region]}:spot-instances-request/*",
      ]
    ])

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${var.machines_tag_prefix}-managed"
      values   = ["true"]
    }
  }

  statement {
    sid       = "LaunchResources"
    actions   = ["ec2:RunInstances"]
    resources = flatten([for region in var.machines_regions : local.launch_resource_arns[region]])
  }

  statement {
    sid     = "TagOnLaunch"
    actions = ["ec2:CreateTags"]
    resources = flatten([
      for region in var.machines_regions : [
        "${local.ec2_arn_prefix[region]}:instance/*",
        "${local.ec2_arn_prefix[region]}:spot-instances-request/*",
      ]
    ])

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["RunInstances"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${var.machines_tag_prefix}-managed"
      values   = ["true"]
    }
  }

  statement {
    sid       = "TerminateManagedInstance"
    actions   = ["ec2:TerminateInstances"]
    resources = [for region in var.machines_regions : "${local.ec2_arn_prefix[region]}:instance/*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/${var.machines_tag_prefix}-managed"
      values   = ["true"]
    }
  }

  statement {
    sid       = "CancelManagedSpotRequest"
    actions   = ["ec2:CancelSpotInstanceRequests"]
    resources = [for region in var.machines_regions : "${local.ec2_arn_prefix[region]}:spot-instances-request/*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/${var.machines_tag_prefix}-managed"
      values   = ["true"]
    }
  }

  # same-account resource policies that name the user can grant past an implicit deny in a
  # boundary, so everything outside EC2 is denied outright
  statement {
    sid         = "NothingButEc2"
    effect      = "Deny"
    not_actions = ["ec2:*"]
    resources   = ["*"]
  }
}

data "aws_iam_policy_document" "rembuild_boundary" {
  statement {
    sid = "Inventory"
    actions = [
      "ec2:DescribeImages",
      "ec2:DescribeInstanceStatus",
      "ec2:DescribeInstanceTypes",
      "ec2:DescribeInstances",
      "ec2:DescribeSecurityGroups",
      "ec2:DescribeSpotInstanceRequests",
      "ec2:DescribeSubnets",
    ]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:Region"
      values   = var.rembuild_regions
    }
  }

  statement {
    sid       = "SharedFirewallIngress"
    actions   = ["ec2:AuthorizeSecurityGroupIngress", "ec2:RevokeSecurityGroupIngress"]
    resources = [for region in var.rembuild_regions : "${local.ec2_arn_prefix[region]}:security-group/*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/rb-managed"
      values   = ["rembuild"]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/rb-kind"
      values   = ["firewall"]
    }
  }

  statement {
    sid     = "LaunchManagedBuilder"
    actions = ["ec2:RunInstances"]
    resources = flatten([
      for region in var.rembuild_regions : [
        "${local.ec2_arn_prefix[region]}:instance/*",
        "${local.ec2_arn_prefix[region]}:spot-instances-request/*",
      ]
    ])

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/rb-managed"
      values   = local.rembuild_managed_values
    }
  }

  statement {
    sid       = "LaunchResources"
    actions   = ["ec2:RunInstances"]
    resources = flatten([for region in var.rembuild_regions : local.launch_resource_arns[region]])
  }

  statement {
    sid     = "TagOnLaunch"
    actions = ["ec2:CreateTags"]
    resources = flatten([
      for region in var.rembuild_regions : [
        "${local.ec2_arn_prefix[region]}:instance/*",
        "${local.ec2_arn_prefix[region]}:spot-instances-request/*",
      ]
    ])

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["RunInstances"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/rb-managed"
      values   = local.rembuild_managed_values
    }
  }

  statement {
    sid       = "ManagedBuilderLifecycle"
    actions   = ["ec2:StartInstances", "ec2:StopInstances", "ec2:TerminateInstances"]
    resources = [for region in var.rembuild_regions : "${local.ec2_arn_prefix[region]}:instance/*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/rb-managed"
      values   = local.rembuild_managed_values
    }
  }

  statement {
    sid       = "ManagedBuilderUserData"
    actions   = ["ec2:ModifyInstanceAttribute"]
    resources = [for region in var.rembuild_regions : "${local.ec2_arn_prefix[region]}:instance/*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/rb-managed"
      values   = local.rembuild_managed_values
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:Attribute"
      values   = ["UserData"]
    }
  }

  statement {
    sid       = "CancelManagedSpotRequest"
    actions   = ["ec2:CancelSpotInstanceRequests"]
    resources = [for region in var.rembuild_regions : "${local.ec2_arn_prefix[region]}:spot-instances-request/*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/rb-managed"
      values   = local.rembuild_managed_values
    }
  }

  # same-account resource policies that name the user can grant past an implicit deny in a
  # boundary, so everything outside EC2 is denied outright
  statement {
    sid         = "NothingButEc2"
    effect      = "Deny"
    not_actions = ["ec2:*"]
    resources   = ["*"]
  }
}

resource "aws_iam_policy" "boundary" {
  for_each = local.service_users

  name        = each.value.name
  path        = var.boundary_path
  description = "Permissions boundary of ${each.value.name}: the most its policies can grant"
  policy      = each.value.boundary_document

  tags = local.common_tags

  lifecycle {
    precondition {
      condition     = length(each.value.boundary_document) <= 6144
      error_message = "The boundary policy exceeds the AWS managed-policy size limit"
    }
  }
}
