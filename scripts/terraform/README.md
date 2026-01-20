# Sovereign Ligero AWS Infrastructure

Terraform configuration for deploying Sovereign Ligero infrastructure on AWS.

## Infrastructure Overview

### VPC Configuration
- **Region**: us-east-1
- **CIDR Block**: 172.32.0.0/16
- **Subnets** (4 x /18, ~16,382 IPs each):
  | Subnet | CIDR | AZ | Type |
  |--------|------|-----|------|
  | public-1a | 172.32.0.0/18 | us-east-1a | Public |
  | public-1b | 172.32.64.0/18 | us-east-1b | Public |
  | private-1a | 172.32.128.0/18 | us-east-1a | Private |
  | private-1b | 172.32.192.0/18 | us-east-1b | Private |

### EC2 Instance
- **Instance Type**: c7g.4xlarge (16 vCPU, 32 GB RAM, ARM Graviton3)
- **AMI**: Ubuntu 24.04 LTS (ami-07f75595710e1c42b)
- **Storage**: 300 GB GP3 EBS volume
- **Network**: Public subnet with Elastic IP
- **Protection**: Termination protection enabled

## Prerequisites

1. **Terraform**: Install Terraform >= 1.0.0
   ```bash
   brew install terraform  # macOS
   ```

2. **AWS CLI**: Configure with appropriate credentials
   ```bash
   aws configure
   ```

3. **SSH Key Pair**: Create or import an EC2 key pair
   ```bash
   # Create a new key pair
   aws ec2 create-key-pair \
     --key-name sovereign-ligero-keypair \
     --query 'KeyMaterial' \
     --output text > ~/.ssh/sovereign-ligero-keypair.pem
   chmod 400 ~/.ssh/sovereign-ligero-keypair.pem

   # Or import an existing public key
   aws ec2 import-key-pair \
     --key-name sovereign-ligero-keypair \
     --public-key-material fileb://~/.ssh/id_rsa.pub
   ```

## Usage

### Initialize Terraform
```bash
terraform init
```

### Review the Plan
```bash
terraform plan
```

### Apply Configuration
```bash
terraform apply
```

### Connect to Instance
After deployment, use the SSH command from outputs:
```bash
terraform output ssh_connection_command
# Example: ssh -i ~/.ssh/sovereign-ligero-keypair.pem ubuntu@<elastic-ip>
```

### Destroy Infrastructure
```bash
# First, disable termination protection
aws ec2 modify-instance-attribute \
  --instance-id $(terraform output -raw instance_id) \
  --no-disable-api-termination

# Then destroy
terraform destroy
```

## Configuration

### Key Variables (terraform.tfvars)

| Variable | Default | Description |
|----------|---------|-------------|
| `aws_region` | us-east-1 | AWS region |
| `vpc_cidr` | 172.32.0.0/16 | VPC CIDR block |
| `instance_type` | c7g.4xlarge | EC2 instance type |
| `key_pair_name` | sovereign-ligero-keypair | EC2 key pair name |
| `root_volume_size` | 300 | Root volume size in GB |

### Security Groups

**Web Server SG** (attached to EC2):
- Inbound: SSH (22), HTTP (80), HTTPS (443) from 0.0.0.0/0
- Outbound: All traffic

**Default SG** (VPC default):
- Inbound: None
- Outbound: VPC internal only

## Remote State (Optional)

To enable remote state storage, uncomment the backend configuration in `main.tf`:

```hcl
backend "s3" {
  bucket         = "sovereign-ligero-terraform-state"
  key            = "infrastructure/terraform.tfstate"
  region         = "us-east-1"
  encrypt        = true
  dynamodb_table = "terraform-state-lock"
}
```

Create the S3 bucket and DynamoDB table first:
```bash
# Create S3 bucket
aws s3 mb s3://sovereign-ligero-terraform-state --region us-east-1
aws s3api put-bucket-versioning \
  --bucket sovereign-ligero-terraform-state \
  --versioning-configuration Status=Enabled

# Create DynamoDB table for state locking
aws dynamodb create-table \
  --table-name terraform-state-lock \
  --attribute-definitions AttributeName=LockID,AttributeType=S \
  --key-schema AttributeName=LockID,KeyType=HASH \
  --billing-mode PAY_PER_REQUEST \
  --region us-east-1
```

## Outputs

After `terraform apply`, the following outputs are available:

- `vpc_id` - VPC identifier
- `public_subnet_ids` - List of public subnet IDs
- `private_subnet_ids` - List of private subnet IDs
- `instance_id` - EC2 instance ID
- `instance_public_ip` - Elastic IP address
- `ssh_connection_command` - Ready-to-use SSH command

## File Structure

```
terraform/
├── main.tf              # Provider and backend configuration
├── variables.tf         # Variable definitions
├── vpc.tf               # VPC, subnets, IGW, route tables
├── security_groups.tf   # Security group definitions
├── ec2.tf               # EC2 instance and EIP
├── outputs.tf           # Output definitions
├── terraform.tfvars     # Variable values
└── README.md            # This file
```

## Cost Estimation

| Resource | Estimated Monthly Cost |
|----------|----------------------|
| c7g.4xlarge (on-demand) | ~$350 |
| 300 GB GP3 EBS | ~$24 |
| Elastic IP (attached) | $0 |
| Data Transfer | Variable |
| **Total** | **~$375/month** |

Consider Reserved Instances or Savings Plans for production workloads.

## Security Notes

1. **SSH Access**: Currently open to 0.0.0.0/0. Consider restricting to specific IP ranges.
2. **IMDSv2**: Instance metadata service v2 is required (more secure).
3. **Encryption**: Root volume is encrypted by default.
4. **Termination Protection**: Enabled to prevent accidental deletion.

## Support

For issues or questions, contact: nico@dcspark.io
