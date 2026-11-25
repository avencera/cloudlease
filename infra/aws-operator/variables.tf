variable "aws_account_id" {
  description = "AWS account that holds the operator user and the service users"
  type        = string

  validation {
    condition     = can(regex("^[0-9]{12}$", var.aws_account_id))
    error_message = "aws_account_id must contain 12 digits"
  }
}

variable "operator_user_name" {
  description = "IAM user that applies the service users' Terraform and rotates their access keys"
  type        = string
  default     = "local-automation"

  validation {
    condition = (
      length(var.operator_user_name) >= 1 &&
      length(var.operator_user_name) <= 64 &&
      can(regex("^[A-Za-z0-9+=,.@_-]+$", var.operator_user_name))
    )
    error_message = "operator_user_name must be 1 through 64 valid IAM user-name characters"
  }
}

variable "machines_user_name" {
  description = "The machines Worker's IAM user; must match iam_user_name in infra/aws-machines"
  type        = string
  default     = "cloudlease-machines"

  validation {
    condition = (
      length(var.machines_user_name) >= 1 &&
      length(var.machines_user_name) <= 48 &&
      can(regex("^[A-Za-z0-9+=,.@_-]+$", var.machines_user_name))
    )
    error_message = "machines_user_name must be 1 through 48 valid IAM user-name characters"
  }
}

variable "machines_regions" {
  description = "Regions the machines Worker launches in; its boundary allows no other"
  type        = list(string)

  validation {
    condition = length(var.machines_regions) >= 1 && alltrue([
      for region in var.machines_regions : can(regex("^[a-z]{2}(-gov)?-[a-z]+-[0-9]$", region))
    ])
    error_message = "machines_regions must list AWS regions such as us-east-1"
  }
}

variable "machines_tag_prefix" {
  description = "The machines Worker's CLOUDLEASE_TAG_PREFIX; must match tag_prefix in infra/aws-machines"
  type        = string
  default     = "clm"

  validation {
    condition     = can(regex("^[a-z][a-z0-9]{0,15}$", var.machines_tag_prefix))
    error_message = "machines_tag_prefix must be 1 to 16 lowercase letters and digits, starting with a letter"
  }
}

variable "rembuild_user_name" {
  description = "rembuild's Worker IAM user; must match iam_user_name in rembuild's infra/aws-spot-iam"
  type        = string
  default     = "rembuild-worker"

  validation {
    condition = (
      length(var.rembuild_user_name) >= 1 &&
      length(var.rembuild_user_name) <= 48 &&
      can(regex("^[A-Za-z0-9+=,.@_-]+$", var.rembuild_user_name))
    )
    error_message = "rembuild_user_name must be 1 through 48 valid IAM user-name characters"
  }
}

variable "rembuild_regions" {
  description = "Regions rembuild launches builders in; its boundary allows no other"
  type        = list(string)

  validation {
    condition = length(var.rembuild_regions) >= 1 && alltrue([
      for region in var.rembuild_regions : can(regex("^[a-z]{2}(-gov)?-[a-z]+-[0-9]$", region))
    ])
    error_message = "rembuild_regions must list AWS regions such as us-east-2"
  }
}

variable "service_account_path" {
  description = "IAM path of both service users and their policies"
  type        = string
  default     = "/service-accounts/"

  validation {
    condition     = startswith(var.service_account_path, "/") && endswith(var.service_account_path, "/")
    error_message = "service_account_path must start and end with a slash"
  }
}

# the operator may write policies under service_account_path, so the boundaries and its own
# grants must live elsewhere
variable "boundary_path" {
  description = "IAM path of the permissions boundaries"
  type        = string
  default     = "/boundaries/"

  validation {
    condition = (
      startswith(var.boundary_path, "/") &&
      endswith(var.boundary_path, "/") &&
      var.boundary_path != "/" &&
      !startswith(var.boundary_path, var.service_account_path) &&
      !startswith(var.service_account_path, var.boundary_path)
    )
    error_message = "boundary_path must be a slash-delimited path that neither contains nor lies inside service_account_path"
  }
}

variable "operator_policy_path" {
  description = "IAM path of the operator's managed grants"
  type        = string
  default     = "/operators/"

  validation {
    condition = (
      startswith(var.operator_policy_path, "/") &&
      endswith(var.operator_policy_path, "/") &&
      var.operator_policy_path != "/" &&
      !startswith(var.operator_policy_path, var.service_account_path) &&
      !startswith(var.service_account_path, var.operator_policy_path)
    )
    error_message = "operator_policy_path must be a slash-delimited path that neither contains nor lies inside service_account_path"
  }
}
