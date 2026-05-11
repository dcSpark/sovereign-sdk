# midnight-monitor-lambda

Lambda-friendly monitor for the Ligero deployment behind the nginx routes.

It checks:

- service health endpoints under `BASE_URL/<service>/...`
- `GET BASE_URL/metrics/s5` and requires `PeakTPS > 0`
- `POST BASE_URL/proof-pool/send` with `proof_quantity=1`
- `POST TEE_RESET_URL` without auth and treats any returned HTTP status as proof the endpoint is alive; only timeout or connection-refused failures mark it unhealthy
- `scripts/mcp-external-stress.sh` against `BASE_URL/mcp/mcp` with 3 wallets and 1 tx by default
- `GET BASE_URL/controller/stats` with optional basic auth and emits the selected mount path's disk usage percentage
- direct Mac worker health URLs from `MONITOR_WORKER_HEALTH_URLS`, emitting one check per worker host
- emits CloudWatch Embedded Metric Format events under the `Midnight/Monitor` namespace

## Environment variables

- `BASE_URL` (required), for example `https://midnight-l2-testnet.shinkai.com`
- `MONITOR_ENV` (required), for example `midnight-l2-testnet`
- `MONITOR_CONTROLLER_BASIC_AUTH_USERNAME` / `MONITOR_CONTROLLER_BASIC_AUTH_PASSWORD` (optional together, but needed if `/controller/stats` is protected)
- `MONITOR_DISK_USAGE_MOUNT_PATH` (default `/`)
- `MONITOR_WORKER_HEALTH_URLS` (optional), comma- or newline-separated direct worker health URLs, for example `http://172.33.109.145:8080/health`
- `PROOF_POOL_AUTH_TOKEN` (optional, but needed if `/proof-pool/send` is protected)
- `MONITOR_TEE_RESET_URL` (optional; falls back to `TEE_RESET_URL`, default `http://74.235.106.62:9898/reset`)
- `HTTP_TIMEOUT_SECS` (default `10`)
- `STRESS_WALLETS` (default `3`)
- `STRESS_TXS` (default `1`)
- `MCP_STRESS_SESSION_IDS_FILE` (default `/tmp/mcp-external-stress-session-ids.txt`)
- `MCP_STRESS_SCRIPT_PATH` (default `${LAMBDA_TASK_ROOT}/scripts/mcp-external-stress.sh`)

## Metrics and alerting

Each invocation emits:

- `Heartbeat{Environment}` = `1`
- `RunStatus{Environment}` = `1` when all checks pass, otherwise `0`
- `CheckStatus{Environment,Check}` = `1` or `0` for each monitor check
- `CheckLatencyMs{Environment,Check}` for each monitor check
- `DiskUsagePercent{Environment,MountPath}` for the selected disk mount

The emitted `Check` dimension values are:

- `health:rollup`
- `health:worker`
- `health:fvk`
- `health:indexer`
- `health:mcp`
- `health:metrics`
- `health:oracle`
- `health:proof-pool`
- `health:worker-instance:<host>` for each URL in `MONITOR_WORKER_HEALTH_URLS`
- `metrics:s5-peak-tps`
- `proof-pool:send`
- `tee:reset-endpoint`
- `mcp:stress`
- `system:disk-usage`

The MCP stress check is considered healthy as long as at least one of the configured sends succeeds.
With the default `3` wallets and `1` tx per wallet, it only fails when `0/3` sends succeed.

The Terraform in `scripts/terraform` provisions:

- an ARM64 image-based Lambda in the VPC private subnet for `us-east-1d`
- an EventBridge Scheduler trigger every 5 minutes
- per-check CloudWatch alarms plus Lambda error, heartbeat-missing, and scheduler DLQ alarms
- SNS -> AWS Chatbot Slack delivery for state-change notifications

## Build

```bash
docker buildx build \
  --platform linux/amd64 \
  --provenance=false \
  -f crates/utils/midnight-monitor-lambda/Dockerfile \
  -t midnight-monitor-lambda:latest .
```

Use `linux/arm64` instead if the Lambda function will run on Graviton.

## Deployment prerequisites

Before applying the Terraform monitor resources, provide:

- a pushed ARM64 image tag in ECR for `midnight-monitor-lambda`
- `monitor_proof_pool_auth_token`
- `monitor_tee_reset_url` plus matching network egress settings if the TEE endpoint changes
- `slack_workspace_id`
- `slack_channel_id`
- `slack_channel_name`
