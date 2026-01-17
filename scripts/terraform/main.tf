# =============================================================================
# Terraform Configuration for Sovereign Ligero AWS Infrastructure
# =============================================================================

terraform {
  required_version = ">= 1.0.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }

  # Remote state storage configuration
  backend "s3" {
    bucket       = "midnight-rollup-terraform-state"
    key          = "infrastructure/terraform.tfstate"
    region       = "us-east-1"
    encrypt      = true
    use_lockfile = true
  }
}

provider "aws" {
  region  = var.aws_region
  profile = var.aws_profile

  default_tags {
    tags = {
      Project     = "midnight-rollup"
      Environment = var.environment
      ManagedBy   = "terraform"
      Owner       = "dcspark.io"
    }
  }
}
