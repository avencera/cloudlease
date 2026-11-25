data "aws_partition" "current" {}

locals {
  iam_arn_prefix = "arn:${data.aws_partition.current.partition}:iam::${var.aws_account_id}"

  # built from names rather than read from infra/aws-machines, so the grant also covers
  # creating the user and its policies from nothing
  worker_user_arn    = "${local.iam_arn_prefix}:user${var.worker_iam_path}${var.worker_iam_user_name}"
  worker_policy_arns = "${local.iam_arn_prefix}:policy${var.worker_iam_path}${var.worker_iam_user_name}-*"
}

data "aws_iam_policy_document" "operator" {
  # reads are scoped here too, so the grant does not lean on another app's operator policy
  statement {
    sid       = "ReadMachinesIdentity"
    effect    = "Allow"
    actions   = ["iam:Get*", "iam:List*"]
    resources = [local.worker_user_arn, local.worker_policy_arns]
  }

  # the same writes the operator holds for rembuild's worker user, and only on these names
  statement {
    sid    = "ManageMachinesIdentity"
    effect = "Allow"
    actions = [
      "iam:AttachUserPolicy",
      "iam:CreatePolicy",
      "iam:CreatePolicyVersion",
      "iam:CreateUser",
      "iam:DeletePolicy",
      "iam:DeletePolicyVersion",
      "iam:DeleteUser",
      "iam:DetachUserPolicy",
      "iam:TagPolicy",
      "iam:TagUser",
      "iam:UntagPolicy",
      "iam:UntagUser",
      "iam:UpdateUser",
    ]
    resources = [local.worker_user_arn, local.worker_policy_arns]
  }

  # the access key stays out of Terraform state, so it is created and rotated with the CLI
  statement {
    sid    = "RotateMachinesAccessKey"
    effect = "Allow"
    actions = [
      "iam:CreateAccessKey",
      "iam:DeleteAccessKey",
      "iam:ListAccessKeys",
    ]
    resources = [local.worker_user_arn]
  }
}

# inline on the operator user, like its grant for rembuild's worker user; the operator holds no
# iam:PutUserPolicy on itself, so only an administrator can widen this
resource "aws_iam_user_policy" "operator" {
  name   = "cloudlease-iam-operator"
  user   = var.operator_user_name
  policy = data.aws_iam_policy_document.operator.json
}
