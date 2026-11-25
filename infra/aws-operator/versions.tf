terraform {
  required_version = ">= 1.9.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
}

# IAM is global; the provider still needs a region for its endpoints
provider "aws" {
  region              = "us-east-1"
  allowed_account_ids = [var.aws_account_id]
}
