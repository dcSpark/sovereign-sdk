# =============================================================================
# Output Values
# =============================================================================

# -----------------------------------------------------------------------------
# VPC Outputs
# -----------------------------------------------------------------------------

output "vpc_id" {
  description = "ID of the VPC"
  value       = aws_vpc.main.id
}

output "vpc_cidr" {
  description = "CIDR block of the VPC"
  value       = aws_vpc.main.cidr_block
}

# -----------------------------------------------------------------------------
# Subnet Outputs
# -----------------------------------------------------------------------------

output "public_subnet_ids" {
  description = "IDs of the public subnets"
  value       = aws_subnet.public[*].id
}

output "private_subnet_ids" {
  description = "IDs of the private subnets"
  value       = aws_subnet.private[*].id
}

output "public_subnet_cidrs" {
  description = "CIDR blocks of the public subnets"
  value       = aws_subnet.public[*].cidr_block
}

output "private_subnet_cidrs" {
  description = "CIDR blocks of the private subnets"
  value       = aws_subnet.private[*].cidr_block
}

# -----------------------------------------------------------------------------
# Internet Gateway Output
# -----------------------------------------------------------------------------

output "internet_gateway_id" {
  description = "ID of the Internet Gateway"
  value       = aws_internet_gateway.main.id
}

# -----------------------------------------------------------------------------
# NAT Gateway Outputs
# -----------------------------------------------------------------------------

output "nat_gateway_id" {
  description = "ID of the NAT Gateway"
  value       = aws_nat_gateway.main.id
}

output "nat_gateway_public_ip" {
  description = "Public IP address of the NAT Gateway"
  value       = aws_eip.nat.public_ip
}

# -----------------------------------------------------------------------------
# Security Group Outputs
# -----------------------------------------------------------------------------

output "default_security_group_id" {
  description = "ID of the default security group"
  value       = aws_default_security_group.default.id
}

output "web_server_security_group_id" {
  description = "ID of the web server security group"
  value       = aws_security_group.web_server.id
}

# -----------------------------------------------------------------------------
# EC2 Instance Outputs
# -----------------------------------------------------------------------------

output "instance_id" {
  description = "ID of the EC2 instance"
  value       = aws_instance.main.id
}

output "instance_private_ip" {
  description = "Private IP address of the EC2 instance"
  value       = aws_instance.main.private_ip
}

output "instance_public_ip" {
  description = "Public IP address of the EC2 instance"
  value       = aws_instance.main.public_ip
}

output "instance_public_dns" {
  description = "Public DNS name of the EC2 instance"
  value       = aws_instance.main.public_dns
}

# -----------------------------------------------------------------------------
# Connection Information
# -----------------------------------------------------------------------------

output "ssh_connection_command" {
  description = "SSH command to connect to the instance"
  value       = "ssh -i <path-to-private-key> ubuntu@${aws_instance.main.public_ip}"
}

# -----------------------------------------------------------------------------
# RDS Outputs
# -----------------------------------------------------------------------------

output "rds_endpoint" {
  description = "RDS instance endpoint (hostname:port)"
  value       = aws_db_instance.main.endpoint
}

output "rds_address" {
  description = "RDS instance hostname"
  value       = aws_db_instance.main.address
}

output "rds_port" {
  description = "RDS instance port"
  value       = aws_db_instance.main.port
}

output "rds_database_name" {
  description = "Name of the default database"
  value       = aws_db_instance.main.db_name
}

output "rds_username" {
  description = "Master username for the RDS instance"
  value       = aws_db_instance.main.username
  sensitive   = true
}

output "rds_security_group_id" {
  description = "ID of the RDS security group"
  value       = aws_security_group.rds_postgres.id
}

output "rds_connection_string" {
  description = "PostgreSQL connection string (password not included)"
  value       = "postgresql://${aws_db_instance.main.username}@${aws_db_instance.main.address}:${aws_db_instance.main.port}/${aws_db_instance.main.db_name}"
}
