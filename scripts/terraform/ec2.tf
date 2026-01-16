# =============================================================================
# EC2 Instance and Related Resources
# =============================================================================

# -----------------------------------------------------------------------------
# EC2 Instance
# -----------------------------------------------------------------------------

resource "aws_instance" "main" {
  ami                     = var.ami_id
  instance_type           = var.instance_type
  key_name                = var.key_pair_name
  subnet_id               = aws_subnet.public[0].id # us-east-1a public subnet
  vpc_security_group_ids  = [aws_security_group.web_server.id]
  disable_api_termination = true # Termination protection enabled

  # Root volume configuration
  root_block_device {
    volume_size           = var.root_volume_size
    volume_type           = var.root_volume_type
    iops                  = var.root_volume_iops
    throughput            = var.root_volume_throughput
    encrypted             = true
    delete_on_termination = true
  }

  # Instance metadata options (IMDSv2 required for security)
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  # Enable detailed monitoring
  monitoring = true

  tags = {
    Name = "${var.project_name}-instance"
  }

  lifecycle {
    # Prevent accidental destruction
    prevent_destroy = false # Set to true in production
  }
}

# -----------------------------------------------------------------------------
# Elastic IP (commented out - EIP limit reached)
# -----------------------------------------------------------------------------

# resource "aws_eip" "main" {
#   domain = "vpc"
#
#   tags = {
#     Name = "${var.project_name}-eip"
#   }
# }
#
# resource "aws_eip_association" "main" {
#   instance_id   = aws_instance.main.id
#   allocation_id = aws_eip.main.id
# }
