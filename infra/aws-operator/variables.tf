variable "aws_account_id" {
  description = "AWS account that holds the operator user and the machines Worker's IAM user"
  type        = string

  validation {
    condition     = can(regex("^[0-9]{12}$", var.aws_account_id))
    error_message = "aws_account_id must contain 12 digits"
  }
}

variable "operator_user_name" {
  description = "IAM user that applies infra/aws-machines and rotates the Worker's access key"
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

variable "worker_iam_user_name" {
  description = "The machines Worker's IAM user; must match iam_user_name in infra/aws-machines"
  type        = string
  default     = "cloudlease-machines"

  validation {
    condition = (
      length(var.worker_iam_user_name) >= 1 &&
      length(var.worker_iam_user_name) <= 48 &&
      can(regex("^[A-Za-z0-9+=,.@_-]+$", var.worker_iam_user_name))
    )
    error_message = "worker_iam_user_name must be 1 through 48 valid IAM user-name characters"
  }
}

variable "worker_iam_path" {
  description = "IAM path of the Worker's user and policies; must match iam_path in infra/aws-machines"
  type        = string
  default     = "/service-accounts/"

  validation {
    condition     = startswith(var.worker_iam_path, "/") && endswith(var.worker_iam_path, "/")
    error_message = "worker_iam_path must start and end with a slash"
  }
}
