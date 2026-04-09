# =============================================================================
# Terraform Variable Values
# =============================================================================

# AWS Configuration
aws_region   = "us-east-1"
environment  = "staging"
project_name = "midnight-rollup"

# AWS Profile (uncomment and set if not using default profile)
# aws_profile = "midnight"

# VPC Configuration
vpc_cidr           = "172.32.0.0/16"
vpc_secondary_cidr = "172.33.0.0/16" # Additional CIDR for us-east-1c and 1d
availability_zones = ["us-east-1a", "us-east-1b", "us-east-1c", "us-east-1d"]

# Subnet CIDRs
# - Original subnets (1a, 1b) use primary CIDR 172.32.x.x - DO NOT CHANGE
# - New subnets (1c, 1d) use secondary CIDR 172.33.x.x
# Each /18 subnet provides 16,382 usable IP addresses
public_subnet_cidrs = [
  "172.32.0.0/18",  # us-east-1a (existing)
  "172.32.64.0/18", # us-east-1b (existing)
  "172.33.0.0/18",  # us-east-1c (new)
  "172.33.64.0/18"  # us-east-1d (new)
]
private_subnet_cidrs = [
  "172.32.128.0/18", # us-east-1a (existing)
  "172.32.192.0/18", # us-east-1b (existing)
  "172.33.128.0/18", # us-east-1c (new)
  "172.33.192.0/18"  # us-east-1d (new)
]

# EC2 Configuration
# NOTE: c7g instances are ARM (Graviton3) - must use ARM64 AMI
instance_type = "c7g.4xlarge"
ami_id        = "ami-0071c8c431eea0edb" # Ubuntu 24.04 LTS ARM64 (2025-12-12)

# IMPORTANT: Update this to your actual key pair name
# You can create a key pair in AWS Console or via CLI:
#   aws ec2 create-key-pair --key-name sovereign-ligero-keypair --query 'KeyMaterial' --output text > ~/.ssh/sovereign-ligero-keypair.pem
#   chmod 400 ~/.ssh/sovereign-ligero-keypair.pem
key_pair_name = "sovereign-deploy"

# EBS Volume Configuration
root_volume_size       = 300
root_volume_type       = "gp3"
root_volume_iops       = 3000 # GP3 default: 3000, max: 16000
root_volume_throughput = 125  # GP3 default: 125 MB/s, max: 1000 MB/s

# RDS Configuration
rds_engine_version        = "16.6" # PostgreSQL version (check AWS for available versions)
rds_instance_class        = "db.m8g.large"
rds_allocated_storage     = 100 # Initial storage in GB
rds_max_allocated_storage = 500 # Max storage for autoscaling
rds_database_name         = "midnight"
rds_master_username       = "postgres"
rds_skip_final_snapshot   = true  # Set to false for production
rds_deletion_protection   = false # Set to true for production

# IMPORTANT: Set the RDS password via environment variable or prompt:
#   export TF_VAR_rds_master_password="your-secure-password"
# Or pass it directly:
#   terraform apply -var="rds_master_password=your-secure-password"

# Midnight monitor configuration
monitor_base_url            = "http://172.33.91.192"
monitor_env                 = "midnight-l2-testnet"
monitor_lambda_image_tag    = "arm64-20260323-132009"
monitor_tee_reset_url       = "http://74.235.106.62:9898/reset"
monitor_tee_reset_host_cidr = "74.235.106.62/32"
monitor_tee_reset_port      = 9898

# IMPORTANT: Set the monitor auth token and Slack identifiers via environment variables
# or a non-checked-in tfvars override file before applying:
#   export TF_VAR_monitor_proof_pool_auth_token="..."
#   export TF_VAR_slack_workspace_id="T01234567"
#   export TF_VAR_slack_channel_id="C01234567"
#   export TF_VAR_slack_channel_name="midnight-alerts"
