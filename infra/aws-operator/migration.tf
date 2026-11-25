# Temporary, for one targeted apply: imports rembuild-iam-operator, the inline grant made by
# hand before any Terraform managed it, alongside the boundaries. Both service users then get
# their boundaries, and the next apply, without this file, attaches the managed grants and
# deletes both inline ones, so the new grants never apply to a user without its boundary

import {
  to = aws_iam_user_policy.rembuild_operator_inline
  id = "${var.operator_user_name}:rembuild-iam-operator"
}

data "aws_iam_policy_document" "rembuild_operator_inline" {
  statement {
    sid       = "ReadIamInventory"
    actions   = ["iam:Get*", "iam:List*"]
    resources = ["*"]
  }

  statement {
    sid = "ManageRembuildWorkerIdentity"
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
    resources = [local.service_users.rembuild.user_arn, local.service_users.rembuild.policy_arns]
  }
}

resource "aws_iam_user_policy" "rembuild_operator_inline" {
  name   = "rembuild-iam-operator"
  user   = var.operator_user_name
  policy = data.aws_iam_policy_document.rembuild_operator_inline.json
}

data "aws_iam_policy_document" "machines_operator_inline" {
  statement {
    sid       = "ReadMachinesIdentity"
    effect    = "Allow"
    actions   = ["iam:Get*", "iam:List*"]
    resources = [local.service_users.machines.user_arn, local.service_users.machines.policy_arns]
  }

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
    resources = [local.service_users.machines.user_arn, local.service_users.machines.policy_arns]
  }

  statement {
    sid    = "RotateMachinesAccessKey"
    effect = "Allow"
    actions = [
      "iam:CreateAccessKey",
      "iam:DeleteAccessKey",
      "iam:ListAccessKeys",
    ]
    resources = [local.service_users.machines.user_arn]
  }
}

resource "aws_iam_user_policy" "operator" {
  name   = "cloudlease-iam-operator"
  user   = var.operator_user_name
  policy = data.aws_iam_policy_document.machines_operator_inline.json
}
