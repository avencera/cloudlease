output "boundary_arns" {
  description = "Permissions boundary of each service user; infra/aws-machines and rembuild's infra/aws-spot-iam take these as permissions_boundary_arn"
  value       = { for key, user in local.service_users : user.name => aws_iam_policy.boundary[key].arn }
}

output "operator_policy_arns" {
  description = "Managed grants attached to the operator user"
  value       = { for name, policy in aws_iam_policy.operator : name => policy.arn }
}
