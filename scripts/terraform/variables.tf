# =============================================================================
# Variable Definitions
# =============================================================================

variable "aws_region" {
  description = "AWS region for deployment"
  type        = string
  default     = "us-east-1"
}

variable "aws_profile" {
  description = "AWS CLI profile to use (null uses default profile or environment variables)"
  type        = string
  default     = null
}

variable "environment" {
  description = "Environment name (e.g., dev, staging, prod)"
  type        = string
  default     = "staging"
}

variable "project_name" {
  description = "Project name used for resource naming"
  type        = string
  default     = "midnight-rollup"
}

# -----------------------------------------------------------------------------
# VPC Configuration
# -----------------------------------------------------------------------------

variable "vpc_cidr" {
  description = "CIDR block for the VPC"
  type        = string
  default     = "172.32.0.0/16"
}

variable "availability_zones" {
  description = "Availability zones to use"
  type        = list(string)
  default     = ["us-east-1a", "us-east-1b", "us-east-1c", "us-east-1d"]
}

# Secondary CIDR block for additional subnets (us-east-1c, us-east-1d)
variable "vpc_secondary_cidr" {
  description = "Secondary CIDR block for the VPC to accommodate additional AZs"
  type        = string
  default     = "172.33.0.0/16"
}

# Subnet CIDRs - Original /18 subnets preserved, new AZs use secondary CIDR
# IMPORTANT: Do NOT change existing subnet CIDRs or you will destroy running resources!
variable "public_subnet_cidrs" {
  description = "CIDR blocks for public subnets"
  type        = list(string)
  default     = [
    "172.32.0.0/18",   # us-east-1a (existing - DO NOT CHANGE)
    "172.32.64.0/18",  # us-east-1b (existing - DO NOT CHANGE)
    "172.33.0.0/18",   # us-east-1c (new - from secondary CIDR)
    "172.33.64.0/18"   # us-east-1d (new - from secondary CIDR)
  ]
}

variable "private_subnet_cidrs" {
  description = "CIDR blocks for private subnets"
  type        = list(string)
  default     = [
    "172.32.128.0/18", # us-east-1a (existing - DO NOT CHANGE)
    "172.32.192.0/18", # us-east-1b (existing - DO NOT CHANGE)
    "172.33.128.0/18", # us-east-1c (new - from secondary CIDR)
    "172.33.192.0/18"  # us-east-1d (new - from secondary CIDR)
  ]
}

# -----------------------------------------------------------------------------
# EC2 Configuration
# -----------------------------------------------------------------------------

variable "instance_type" {
  description = "EC2 instance type"
  type        = string
  default     = "c7g.4xlarge"
}

variable "ami_id" {
  description = "AMI ID for the EC2 instance (Ubuntu 24.04 LTS)"
  type        = string
  default     = "ami-07f75595710e1c42b"
}

variable "key_pair_name" {
  description = "Name of the existing EC2 key pair"
  type        = string
  default     = "sovereign-deploy"
}

variable "root_volume_size" {
  description = "Size of the root EBS volume in GB"
  type        = number
  default     = 300
}

variable "root_volume_type" {
  description = "Type of the root EBS volume"
  type        = string
  default     = "gp3"
}

# GP3 specific settings
variable "root_volume_iops" {
  description = "IOPS for GP3 volume (3000-16000)"
  type        = number
  default     = 3000
}

variable "root_volume_throughput" {
  description = "Throughput for GP3 volume in MB/s (125-1000)"
  type        = number
  default     = 125
}
