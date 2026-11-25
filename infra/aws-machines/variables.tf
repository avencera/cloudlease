variable "aws_account_id" {
  description = "AWS account that owns the machines Worker's resources"
  type        = string

  validation {
    condition     = can(regex("^[0-9]{12}$", var.aws_account_id))
    error_message = "aws_account_id must contain 12 digits"
  }
}

variable "aws_region" {
  description = "Region the machines Worker launches Spot instances in; one state per account, since the IAM user is account-wide"
  type        = string

  validation {
    condition     = can(regex("^[a-z]{2}(-gov)?-[a-z]+-[0-9]$", var.aws_region))
    error_message = "aws_region must be an AWS region such as us-east-1"
  }
}

variable "availability_zones" {
  description = "Zones that get a public subnet, in the order AWS classes should try them"
  type        = list(string)

  validation {
    condition = (
      length(var.availability_zones) >= 1 &&
      length(var.availability_zones) <= 3 &&
      length(distinct(var.availability_zones)) == length(var.availability_zones) &&
      alltrue([
        for zone in var.availability_zones : (
          startswith(zone, var.aws_region) && length(zone) == length(var.aws_region) + 1
        )
      ])
    )
    error_message = "availability_zones must be 1 to 3 distinct lettered zones in aws_region"
  }
}

variable "ami_id" {
  description = "Canonical Ubuntu 24.04 amd64 AMI from the SSM public parameter"
  type        = string

  validation {
    condition     = can(regex("^ami-(?:[0-9a-f]{8}|[0-9a-f]{17})$", var.ami_id))
    error_message = "ami_id must be an AWS AMI ID"
  }
}

variable "allowed_instance_type_patterns" {
  description = "EC2 instance-type patterns the machines Worker may launch"
  type        = set(string)
  default     = ["c7a.*", "c8a.*", "m7a.*", "c7i.*"]

  validation {
    condition = (
      length(var.allowed_instance_type_patterns) > 0 &&
      alltrue([
        for pattern in var.allowed_instance_type_patterns : (
          length(pattern) <= 63 &&
          can(regex("^[a-z0-9*?][a-z0-9*?-]*\\.[a-z0-9*?-]+$", pattern))
        )
      ])
    )
    error_message = "allowed_instance_type_patterns must contain valid EC2 instance-type patterns"
  }
}

variable "max_root_volume_gib" {
  description = "Largest root volume RunInstances may create; CreateFleet launches are not capped by it"
  type        = number
  default     = 200

  validation {
    condition = (
      floor(var.max_root_volume_gib) == var.max_root_volume_gib &&
      var.max_root_volume_gib >= 8 &&
      var.max_root_volume_gib <= 500
    )
    error_message = "max_root_volume_gib must be an integer from 8 through 500"
  }
}

variable "tag_prefix" {
  description = "The machines Worker's CLOUDLEASE_TAG_PREFIX"
  type        = string
  default     = "clm"

  validation {
    condition     = can(regex("^[a-z][a-z0-9]{0,15}$", var.tag_prefix))
    error_message = "tag_prefix must be 1 to 16 lowercase letters and digits, starting with a letter"
  }
}

variable "iam_user_name" {
  description = "Dedicated IAM user for the machines Worker"
  type        = string
  default     = "cloudlease-machines"

  validation {
    condition = (
      length(var.iam_user_name) >= 1 &&
      length(var.iam_user_name) <= 48 &&
      can(regex("^[A-Za-z0-9+=,.@_-]+$", var.iam_user_name))
    )
    error_message = "iam_user_name must be 1 through 48 valid IAM user-name characters"
  }
}

variable "iam_path" {
  description = "IAM path for the user and managed policies"
  type        = string
  default     = "/service-accounts/"

  validation {
    condition     = startswith(var.iam_path, "/") && endswith(var.iam_path, "/")
    error_message = "iam_path must start and end with a slash"
  }
}

variable "vpc_cidr" {
  description = "CIDR of the dedicated VPC; each zone gets the next /24 in it"
  type        = string
  default     = "10.66.0.0/16"

  validation {
    condition     = can(cidrsubnet(var.vpc_cidr, 8, 0))
    error_message = "vpc_cidr must be an IPv4 CIDR of /16 or larger"
  }
}

variable "permissions_boundary_arn" {
  description = "The user's permissions boundary, the cloudlease-machines entry of infra/aws-operator's boundary_arns output"
  type        = string

  validation {
    condition     = can(regex("^arn:aws(-[a-z]+)*:iam::[0-9]{12}:policy/.+$", var.permissions_boundary_arn))
    error_message = "permissions_boundary_arn must be an IAM policy ARN"
  }
}
