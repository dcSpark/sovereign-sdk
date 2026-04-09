# =============================================================================
# Midnight Monitor Lambda, Scheduling, and Alerting
# =============================================================================

locals {
  monitor_name              = "midnight-monitor-lambda"
  monitor_scheduler_name    = "midnight-monitor-every-5m"
  monitor_scheduler_role    = "midnight-monitor-scheduler-role"
  monitor_lambda_role       = "midnight-monitor-lambda-role"
  monitor_lambda_sg_name    = "midnight-monitor-lambda-sg"
  monitor_alerts_topic_name = "midnight-monitor-alerts"
  monitor_dlq_name          = "midnight-monitor-scheduler-dlq"
  monitor_slack_config_name = "midnight-monitor-slack"
  monitor_chatbot_role_name = "midnight-monitor-chatbot-role"
  monitor_metrics_namespace = "Midnight/Monitor"
  monitor_target_host_cidr  = "172.33.91.192/32"
  monitor_target_port       = 80
  monitor_private_subnet_1d = aws_subnet.private[index(var.availability_zones, "us-east-1d")].id
  monitor_check_alarm_map = {
    health_rollup     = "health:rollup"
    health_worker     = "health:worker"
    health_fvk        = "health:fvk"
    health_indexer    = "health:indexer"
    health_mcp        = "health:mcp"
    health_metrics    = "health:metrics"
    health_oracle     = "health:oracle"
    health_proof_pool = "health:proof-pool"
    metrics_s5_peak   = "metrics:s5-peak-tps"
    proof_pool_send   = "proof-pool:send"
    tee_reset         = "tee:reset-endpoint"
    mcp_stress        = "mcp:stress"
  }
}

resource "aws_ecr_repository" "monitor" {
  name                 = local.monitor_name
  image_tag_mutability = "MUTABLE"

  image_scanning_configuration {
    scan_on_push = true
  }
}

data "aws_iam_policy_document" "monitor_lambda_assume_role" {
  statement {
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }

    actions = ["sts:AssumeRole"]
  }
}

resource "aws_iam_role" "monitor_lambda" {
  name               = local.monitor_lambda_role
  assume_role_policy = data.aws_iam_policy_document.monitor_lambda_assume_role.json
}

resource "aws_iam_role_policy_attachment" "monitor_lambda_basic" {
  role       = aws_iam_role.monitor_lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "monitor_lambda_vpc" {
  role       = aws_iam_role.monitor_lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_security_group" "monitor_lambda" {
  name        = local.monitor_lambda_sg_name
  description = "Security group for midnight monitor lambda"
  vpc_id      = aws_vpc.main.id

  egress {
    description = "Internal nginx monitor target"
    from_port   = local.monitor_target_port
    to_port     = local.monitor_target_port
    protocol    = "tcp"
    cidr_blocks = [local.monitor_target_host_cidr]
  }

  egress {
    description = "TEE reset endpoint"
    from_port   = var.monitor_tee_reset_port
    to_port     = var.monitor_tee_reset_port
    protocol    = "tcp"
    cidr_blocks = [var.monitor_tee_reset_host_cidr]
  }
}

resource "aws_lambda_function" "monitor" {
  function_name                  = local.monitor_name
  package_type                   = "Image"
  image_uri                      = "${aws_ecr_repository.monitor.repository_url}:${var.monitor_lambda_image_tag}"
  role                           = aws_iam_role.monitor_lambda.arn
  architectures                  = ["arm64"]
  memory_size                    = 1024
  timeout                        = 300
  reserved_concurrent_executions = 1

  environment {
    variables = {
      BASE_URL              = var.monitor_base_url
      MONITOR_ENV           = var.monitor_env
      PROOF_POOL_AUTH_TOKEN = var.monitor_proof_pool_auth_token
      TEE_RESET_URL         = var.monitor_tee_reset_url
      RUST_LOG              = "info"
    }
  }

  vpc_config {
    subnet_ids         = [local.monitor_private_subnet_1d]
    security_group_ids = [aws_security_group.monitor_lambda.id]
  }

  depends_on = [
    aws_iam_role_policy_attachment.monitor_lambda_basic,
    aws_iam_role_policy_attachment.monitor_lambda_vpc,
  ]
}

resource "aws_sqs_queue" "monitor_scheduler_dlq" {
  name = local.monitor_dlq_name
}

data "aws_iam_policy_document" "monitor_scheduler_assume_role" {
  statement {
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["scheduler.amazonaws.com"]
    }

    actions = ["sts:AssumeRole"]
  }
}

data "aws_iam_policy_document" "monitor_scheduler" {
  statement {
    effect = "Allow"
    actions = [
      "lambda:InvokeFunction",
    ]
    resources = [
      aws_lambda_function.monitor.arn,
    ]
  }

  statement {
    effect = "Allow"
    actions = [
      "sqs:SendMessage",
    ]
    resources = [
      aws_sqs_queue.monitor_scheduler_dlq.arn,
    ]
  }
}

resource "aws_iam_role" "monitor_scheduler" {
  name               = local.monitor_scheduler_role
  assume_role_policy = data.aws_iam_policy_document.monitor_scheduler_assume_role.json
}

resource "aws_iam_role_policy" "monitor_scheduler" {
  name   = "${local.monitor_scheduler_role}-policy"
  role   = aws_iam_role.monitor_scheduler.id
  policy = data.aws_iam_policy_document.monitor_scheduler.json
}

resource "aws_scheduler_schedule" "monitor" {
  name                = local.monitor_scheduler_name
  description         = "Runs the Midnight monitor Lambda every five minutes"
  schedule_expression = "rate(5 minutes)"
  state               = "ENABLED"

  flexible_time_window {
    mode = "OFF"
  }

  target {
    arn      = aws_lambda_function.monitor.arn
    role_arn = aws_iam_role.monitor_scheduler.arn
    input    = jsonencode({})

    dead_letter_config {
      arn = aws_sqs_queue.monitor_scheduler_dlq.arn
    }

    retry_policy {
      maximum_event_age_in_seconds = 300
      maximum_retry_attempts       = 2
    }
  }
}

resource "aws_sns_topic" "monitor_alerts" {
  name = local.monitor_alerts_topic_name
}

data "aws_iam_policy_document" "monitor_chatbot_assume_role" {
  statement {
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["chatbot.amazonaws.com"]
    }

    actions = ["sts:AssumeRole"]
  }
}

resource "aws_iam_role" "monitor_chatbot" {
  name               = local.monitor_chatbot_role_name
  assume_role_policy = data.aws_iam_policy_document.monitor_chatbot_assume_role.json
}

resource "aws_iam_role_policy_attachment" "monitor_chatbot_readonly" {
  role       = aws_iam_role.monitor_chatbot.name
  policy_arn = "arn:aws:iam::aws:policy/ReadOnlyAccess"
}

resource "aws_chatbot_slack_channel_configuration" "monitor" {
  configuration_name          = local.monitor_slack_config_name
  iam_role_arn                = aws_iam_role.monitor_chatbot.arn
  slack_team_id               = var.slack_workspace_id
  slack_channel_id            = var.slack_channel_id
  sns_topic_arns              = [aws_sns_topic.monitor_alerts.arn]
  logging_level               = "ERROR"
  guardrail_policy_arns       = ["arn:aws:iam::aws:policy/ReadOnlyAccess"]
  user_authorization_required = false
  tags = {
    SlackChannelName = var.slack_channel_name
  }

  depends_on = [aws_iam_role_policy_attachment.monitor_chatbot_readonly]
}

resource "aws_cloudwatch_metric_alarm" "monitor_checks" {
  for_each = local.monitor_check_alarm_map

  alarm_name          = "${local.monitor_name}-${each.key}-alarm"
  alarm_description   = "Alerts when the Midnight monitor check ${each.value} fails twice in a row"
  namespace           = local.monitor_metrics_namespace
  metric_name         = "CheckStatus"
  statistic           = "Minimum"
  period              = 300
  evaluation_periods  = 2
  datapoints_to_alarm = 2
  threshold           = 1
  comparison_operator = "LessThanThreshold"
  treat_missing_data  = "notBreaching"
  dimensions = {
    Environment = var.monitor_env
    Check       = each.value
  }
  alarm_actions = [aws_sns_topic.monitor_alerts.arn]
  ok_actions    = [aws_sns_topic.monitor_alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "monitor_lambda_errors" {
  alarm_name          = "${local.monitor_name}-lambda-errors-alarm"
  alarm_description   = "Alerts when the Midnight monitor Lambda errors"
  namespace           = "AWS/Lambda"
  metric_name         = "Errors"
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 2
  datapoints_to_alarm = 2
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.monitor.function_name
  }
  alarm_actions = [aws_sns_topic.monitor_alerts.arn]
  ok_actions    = [aws_sns_topic.monitor_alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "monitor_heartbeat_missing" {
  alarm_name          = "${local.monitor_name}-heartbeat-missing-alarm"
  alarm_description   = "Alerts when the Midnight monitor stops emitting heartbeat metrics"
  namespace           = local.monitor_metrics_namespace
  metric_name         = "Heartbeat"
  statistic           = "Sum"
  period              = 900
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  threshold           = 1
  comparison_operator = "LessThanThreshold"
  treat_missing_data  = "breaching"
  dimensions = {
    Environment = var.monitor_env
  }
  alarm_actions = [aws_sns_topic.monitor_alerts.arn]
  ok_actions    = [aws_sns_topic.monitor_alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "monitor_scheduler_dlq_messages" {
  alarm_name          = "${local.monitor_name}-scheduler-dlq-alarm"
  alarm_description   = "Alerts when EventBridge Scheduler cannot deliver monitor invocations"
  namespace           = "AWS/SQS"
  metric_name         = "ApproximateNumberOfMessagesVisible"
  statistic           = "Average"
  period              = 300
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  dimensions = {
    QueueName = aws_sqs_queue.monitor_scheduler_dlq.name
  }
  alarm_actions = [aws_sns_topic.monitor_alerts.arn]
  ok_actions    = [aws_sns_topic.monitor_alerts.arn]
}
