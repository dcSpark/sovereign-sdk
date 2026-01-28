# =============================================================================
# RDS PostgreSQL Database
# =============================================================================

# -----------------------------------------------------------------------------
# DB Subnet Group
# RDS requires a subnet group with subnets in at least 2 AZs, even for single-AZ
# -----------------------------------------------------------------------------

resource "aws_db_subnet_group" "main" {
  name        = "midnight-l2-db-subnet-group"
  description = "Subnet group for midnight-l2-db RDS instance"

  # Use first two public subnets (us-east-1a and us-east-1b)
  subnet_ids = [
    aws_subnet.public[0].id,
    aws_subnet.public[1].id,
  ]

  tags = {
    Name = "midnight-l2-db-subnet-group"
  }
}

# -----------------------------------------------------------------------------
# RDS Security Group
# -----------------------------------------------------------------------------

resource "aws_security_group" "rds_postgres" {
  name        = "midnight-l2-db"
  description = "Security group for midnight-l2-db RDS instance - allows PostgreSQL from VPC"
  vpc_id      = aws_vpc.main.id

  # PostgreSQL access from VPC (primary CIDR)
  ingress {
    description = "PostgreSQL from VPC primary CIDR"
    from_port   = 5432
    to_port     = 5432
    protocol    = "tcp"
    cidr_blocks = [var.vpc_cidr]
  }

  # PostgreSQL access from VPC (secondary CIDR)
  ingress {
    description = "PostgreSQL from VPC secondary CIDR"
    from_port   = 5432
    to_port     = 5432
    protocol    = "tcp"
    cidr_blocks = [var.vpc_secondary_cidr]
  }

  # Allow all outbound traffic
  egress {
    description = "Allow all outbound traffic"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "midnight-l2-db"
  }
}

# -----------------------------------------------------------------------------
# RDS PostgreSQL Instance
# -----------------------------------------------------------------------------

resource "aws_db_instance" "main" {
  # Instance identification
  identifier = "midnight-l2-db"

  # Engine configuration
  engine               = "postgres"
  engine_version       = var.rds_engine_version
  instance_class       = var.rds_instance_class
  parameter_group_name = "default.postgres16"

  # Storage configuration
  allocated_storage     = var.rds_allocated_storage
  max_allocated_storage = var.rds_max_allocated_storage # Enables storage autoscaling
  storage_type          = "gp3"
  storage_encrypted     = true

  # Credentials
  db_name  = var.rds_database_name
  username = var.rds_master_username
  password = var.rds_master_password

  # Network configuration
  db_subnet_group_name   = aws_db_subnet_group.main.name
  vpc_security_group_ids = [aws_security_group.rds_postgres.id]
  publicly_accessible    = true
  port                   = 5432
  availability_zone      = "us-east-1a"

  # Single AZ (not Multi-AZ)
  multi_az = false

  # Authentication
  iam_database_authentication_enabled = false

  # Performance Insights (Standard tier with 7-day retention)
  performance_insights_enabled          = true
  performance_insights_retention_period = 7

  # Monitoring
  monitoring_interval = 60 # Enhanced monitoring every 60 seconds
  monitoring_role_arn = aws_iam_role.rds_monitoring.arn

  # Backup configuration
  backup_retention_period = 7
  backup_window           = "03:00-04:00"
  maintenance_window      = "sun:04:00-sun:05:00"

  # Skip final snapshot for development (set to true for production)
  skip_final_snapshot       = var.rds_skip_final_snapshot
  final_snapshot_identifier = var.rds_skip_final_snapshot ? null : "midnight-l2-db-final-snapshot"
  delete_automated_backups  = true
  deletion_protection       = var.rds_deletion_protection

  # Minor version auto-upgrade
  auto_minor_version_upgrade = true

  # Copy tags to snapshots
  copy_tags_to_snapshot = true

  tags = {
    Name = "midnight-l2-db"
  }
}

# -----------------------------------------------------------------------------
# IAM Role for Enhanced Monitoring
# -----------------------------------------------------------------------------

resource "aws_iam_role" "rds_monitoring" {
  name = "midnight-l2-db-monitoring-role"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Action = "sts:AssumeRole"
        Effect = "Allow"
        Principal = {
          Service = "monitoring.rds.amazonaws.com"
        }
      }
    ]
  })

  tags = {
    Name = "midnight-l2-db-monitoring-role"
  }
}

resource "aws_iam_role_policy_attachment" "rds_monitoring" {
  role       = aws_iam_role.rds_monitoring.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonRDSEnhancedMonitoringRole"
}
