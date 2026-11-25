data "aws_partition" "current" {}

data "aws_iam_user" "operator" {
  user_name = var.operator_user_name
}

locals {
  partition      = data.aws_partition.current.partition
  iam_arn_prefix = "arn:${local.partition}:iam::${var.aws_account_id}"

  common_tags = {
    ManagedBy = "Terraform"
    Service   = "cloudlease-aws-operator"
  }

  # ARNs are built from names rather than read from the users' own configurations, so the
  # grants also cover creating each user and its policies from nothing
  service_users = {
    machines = {
      name                 = var.machines_user_name
      operator_policy_name = "cloudlease-iam-operator"
      boundary_document    = data.aws_iam_policy_document.machines_boundary.json
      user_arn             = "${local.iam_arn_prefix}:user${var.service_account_path}${var.machines_user_name}"
      policy_arns          = "${local.iam_arn_prefix}:policy${var.service_account_path}${var.machines_user_name}-*"
      boundary_arn         = "${local.iam_arn_prefix}:policy${var.boundary_path}${var.machines_user_name}"
    }

    rembuild = {
      name                 = var.rembuild_user_name
      operator_policy_name = "rembuild-iam-operator"
      boundary_document    = data.aws_iam_policy_document.rembuild_boundary.json
      user_arn             = "${local.iam_arn_prefix}:user${var.service_account_path}${var.rembuild_user_name}"
      policy_arns          = "${local.iam_arn_prefix}:policy${var.service_account_path}${var.rembuild_user_name}-*"
      boundary_arn         = "${local.iam_arn_prefix}:policy${var.boundary_path}${var.rembuild_user_name}"
    }
  }
}

# One grant per service user. Every write that can give the user permissions requires the
# user's own boundary, so whatever the operator puts in the user's policies stays inside it
data "aws_iam_policy_document" "operator" {
  for_each = local.service_users

  statement {
    sid       = "CreateOrBoundUserOnlyWithItsBoundary"
    actions   = ["iam:CreateUser", "iam:PutUserPermissionsBoundary"]
    resources = [each.value.user_arn]

    condition {
      test     = "StringEquals"
      variable = "iam:PermissionsBoundary"
      values   = [each.value.boundary_arn]
    }
  }

  # only the user's own policies, so AdministratorAccess or another app's policy is refused
  # outright rather than merely capped
  statement {
    sid       = "AttachOwnPoliciesInsideBoundary"
    actions   = ["iam:AttachUserPolicy"]
    resources = [each.value.user_arn]

    condition {
      test     = "StringEquals"
      variable = "iam:PermissionsBoundary"
      values   = [each.value.boundary_arn]
    }

    condition {
      test     = "ArnLike"
      variable = "iam:PolicyARN"
      values   = [each.value.policy_arns]
    }
  }

  # none of these can add a permission
  statement {
    sid = "ManageUser"
    actions = [
      "iam:DeleteUser",
      "iam:DetachUserPolicy",
      "iam:TagUser",
      "iam:UntagUser",
      "iam:UpdateUser",
    ]
    resources = [each.value.user_arn]
  }

  # a policy version may say anything, but takes effect only through the user, under its
  # boundary
  statement {
    sid = "ManageOwnPolicies"
    actions = [
      "iam:CreatePolicy",
      "iam:CreatePolicyVersion",
      "iam:DeletePolicy",
      "iam:DeletePolicyVersion",
      "iam:TagPolicy",
      "iam:UntagPolicy",
    ]
    resources = [each.value.policy_arns]
  }

  # repeats the allow conditions as a deny, so a grant added later by mistake still cannot give
  # the user another boundary, or a policy while it has none (a missing key also matches)
  statement {
    sid    = "OnlyItsOwnBoundary"
    effect = "Deny"
    actions = [
      "iam:AttachUserPolicy",
      "iam:CreateUser",
      "iam:PutUserPermissionsBoundary",
      "iam:PutUserPolicy",
    ]
    resources = [each.value.user_arn]

    condition {
      test     = "StringNotEquals"
      variable = "iam:PermissionsBoundary"
      values   = [each.value.boundary_arn]
    }
  }

  # the access key stays out of Terraform state, so it is created and rotated with the CLI
  statement {
    sid = "RotateAccessKey"
    actions = [
      "iam:CreateAccessKey",
      "iam:DeleteAccessKey",
      "iam:ListAccessKeys",
      "iam:UpdateAccessKey",
    ]
    resources = [each.value.user_arn]
  }
}

data "aws_iam_policy_document" "guardrails" {
  # reads reveal no secret and let the operator check its own grants with the simulator
  statement {
    sid = "ReadIam"
    actions = [
      "iam:Get*",
      "iam:List*",
      "iam:SimulateCustomPolicy",
      "iam:SimulatePrincipalPolicy",
    ]
    resources = ["*"]
  }

  # the denies repeat what the grants already leave out, so a grant added later by mistake
  # still cannot lift a boundary or rewrite these policies
  statement {
    sid       = "KeepBoundaries"
    effect    = "Deny"
    actions   = ["iam:DeleteRolePermissionsBoundary", "iam:DeleteUserPermissionsBoundary"]
    resources = ["*"]
  }

  statement {
    sid    = "KeepBoundaryAndOperatorPolicies"
    effect = "Deny"
    actions = [
      "iam:CreatePolicyVersion",
      "iam:DeletePolicy",
      "iam:DeletePolicyVersion",
      "iam:SetDefaultPolicyVersion",
      "iam:TagPolicy",
      "iam:UntagPolicy",
    ]
    resources = [
      "${local.iam_arn_prefix}:policy${var.boundary_path}*",
      "${local.iam_arn_prefix}:policy${var.operator_policy_path}*",
    ]
  }

  statement {
    sid         = "NoChangesToOperator"
    effect      = "Deny"
    not_actions = ["iam:Get*", "iam:List*", "iam:Simulate*"]
    resources   = [data.aws_iam_user.operator.arn]
  }

  # PowerUserAccess allows sts:AssumeRole on any role, and the CDK bootstrap's deploy role
  # trusts the whole account and may pass cfn-exec-role (AdministratorAccess) to CloudFormation
  statement {
    sid       = "NoCdkBootstrapRoles"
    effect    = "Deny"
    actions   = ["sts:AssumeRole"]
    resources = ["${local.iam_arn_prefix}:role/cdk-*"]
  }

  # CloudFormation runs a stack's later operations with the service role it already has, and
  # the caller needs no iam:PassRole for that, so these stacks would act as administrator
  dynamic "statement" {
    for_each = length(var.admin_role_stack_names) > 0 ? [var.admin_role_stack_names] : []

    content {
      sid    = "NoWritesToAdminRoleStacks"
      effect = "Deny"
      not_actions = [
        "cloudformation:Describe*",
        "cloudformation:Detect*",
        "cloudformation:Get*",
        "cloudformation:List*",
      ]
      resources = [
        for stack in statement.value : "arn:${local.partition}:cloudformation:*:${var.aws_account_id}:stack/${stack}/*"
      ]
    }
  }
}

locals {
  operator_policies = merge(
    { for key, user in local.service_users : user.operator_policy_name => data.aws_iam_policy_document.operator[key].json },
    { "iam-operator-guardrails" = data.aws_iam_policy_document.guardrails.json },
  )
}

# managed rather than inline: AWS caps a user's inline policies at 2,048 characters in total
resource "aws_iam_policy" "operator" {
  for_each = local.operator_policies

  name        = each.key
  path        = var.operator_policy_path
  description = "IAM grant of ${var.operator_user_name} for the cloudlease and rembuild service users"
  policy      = each.value

  tags = local.common_tags

  lifecycle {
    precondition {
      condition     = length(each.value) <= 6144
      error_message = "The operator policy exceeds the AWS managed-policy size limit"
    }
  }
}

resource "aws_iam_user_policy_attachment" "operator" {
  for_each = aws_iam_policy.operator

  user       = var.operator_user_name
  policy_arn = each.value.arn
}
