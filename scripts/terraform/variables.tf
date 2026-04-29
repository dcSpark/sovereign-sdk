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
  default = [
    "172.32.0.0/18",  # us-east-1a (existing - DO NOT CHANGE)
    "172.32.64.0/18", # us-east-1b (existing - DO NOT CHANGE)
    "172.33.0.0/18",  # us-east-1c (new - from secondary CIDR)
    "172.33.64.0/18"  # us-east-1d (new - from secondary CIDR)
  ]
}

variable "private_subnet_cidrs" {
  description = "CIDR blocks for private subnets"
  type        = list(string)
  default = [
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

# -----------------------------------------------------------------------------
# RDS Configuration
# -----------------------------------------------------------------------------

variable "rds_engine_version" {
  description = "PostgreSQL engine version for RDS"
  type        = string
  default     = "16.6" # Adjust to your preferred version (e.g., 16.1, 16.4, etc.)
}

variable "rds_instance_class" {
  description = "RDS instance class"
  type        = string
  default     = "db.m8g.large"
}

variable "rds_allocated_storage" {
  description = "Initial allocated storage in GB"
  type        = number
  default     = 100
}

variable "rds_max_allocated_storage" {
  description = "Maximum storage in GB for autoscaling (set higher than allocated_storage to enable)"
  type        = number
  default     = 500
}

variable "rds_database_name" {
  description = "Name of the default database to create"
  type        = string
  default     = "midnight"
}

variable "rds_master_username" {
  description = "Master username for the RDS instance"
  type        = string
  default     = "postgres"
}

variable "rds_master_password" {
  description = "Master password for the RDS instance"
  type        = string
  sensitive   = true
}

variable "rds_skip_final_snapshot" {
  description = "Skip final snapshot when destroying the database (set to false for production)"
  type        = bool
  default     = true
}

variable "rds_deletion_protection" {
  description = "Enable deletion protection (set to true for production)"
  type        = bool
  default     = false
}

# -----------------------------------------------------------------------------
# Midnight Monitor Configuration
# -----------------------------------------------------------------------------

variable "monitor_base_url" {
  description = "Base URL the monitor Lambda should probe through nginx"
  type        = string
  default     = "http://172.33.91.192"
}

variable "monitor_env" {
  description = "Environment dimension emitted with Midnight monitor metrics"
  type        = string
  default     = "midnight-l2-testnet"
}

variable "monitor_lambda_image_tag" {
  description = "ECR image tag to deploy for the ARM64 monitor Lambda"
  type        = string
  default     = "arm64-20260318-2051"
}

variable "monitor_proof_pool_auth_token" {
  description = "Auth token passed to the proof-pool send check"
  type        = string
  sensitive   = true
}

variable "monitor_tee_reset_url" {
  description = "TEE reset endpoint URL probed by the monitor Lambda"
  type        = string
  default     = "http://74.235.106.62:9898/reset"
}

variable "monitor_tee_reset_host_cidr" {
  description = "CIDR the monitor Lambda may use for the TEE reset endpoint"
  type        = string
  default     = "74.235.106.62/32"
}

variable "monitor_tee_reset_port" {
  description = "TCP port the monitor Lambda may use for the TEE reset endpoint"
  type        = number
  default     = 9898
}

variable "monitor_disk_usage_mount_path" {
  description = "Mount path selected from /controller/stats for disk usage collection"
  type        = string
  default     = "/"
}

variable "slack_workspace_id" {
  description = "Slack workspace/team ID for the AWS Chatbot channel configuration"
  type        = string
}

variable "slack_channel_id" {
  description = "Slack channel ID for the AWS Chatbot channel configuration"
  type        = string
}

variable "slack_channel_name" {
  description = "Human-readable Slack channel name for the AWS Chatbot configuration"
  type        = string
}
