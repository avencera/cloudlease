output "operator_policy" {
  description = "The operator user and the name of its inline grant"
  value = {
    user   = aws_iam_user_policy.operator.user
    policy = aws_iam_user_policy.operator.name
  }
}
