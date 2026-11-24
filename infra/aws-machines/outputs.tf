output "iam_user_name" {
  description = "The machines Worker's IAM user; create its access key outside Terraform"
  value       = aws_iam_user.worker.name
}

output "iam_policy_arns" {
  description = "Managed policies attached to the IAM user"
  value       = { for name, policy in aws_iam_policy.worker : name => policy.arn }
}

output "security_group_id" {
  description = "The outbound-only security group AWS classes launch into"
  value       = aws_security_group.machines.id
}

output "class_fields" {
  description = "The region, zones, AMI and security group an AWS class in MACHINES_CONFIG names"
  value = {
    provider          = "aws"
    region            = var.aws_region
    image_id          = var.ami_id
    root_device_name  = data.aws_ami.ubuntu.root_device_name
    security_group_id = aws_security_group.machines.id
    zones = [
      for zone in var.availability_zones : {
        zone      = zone
        subnet_id = aws_subnet.machines[zone].id
      }
    ]
  }
}
