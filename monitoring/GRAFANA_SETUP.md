# Grafana Setup for Parallel Executor Metrics

## Quick Start

### 1. Launch the Stack

```bash
# From the repository root
./monitoring/launch-monitoring.sh
# or
docker compose -f monitoring/docker-compose.yml up -d
```

This will start:
- **Telegraf** on UDP port 8094 (metrics input) and port 9273 (Prometheus endpoint)
- **Prometheus** on port 9090
- **Grafana** on port 3000

### 2. Access Grafana

Open your browser to: http://localhost:3000

**Default credentials:**
- Username: `admin`
- Password: `admin`

### 3. View the Dashboard

The "Parallel Executor Performance" dashboard is automatically provisioned.

To access it:
1. Go to **Dashboards** → **Browse**
2. Open the **Sequencer** folder
3. Click on **Parallel Executor Performance**

---

## What You'll See

The dashboard includes 7 panels:

### 1. **Average Stage Duration**
- Shows average time for each stage over time
- Stage 1 (green): Should be < 0.1ms
- Stage 2 (blue): Varies widely (1-5000ms)
- Stage 3 (orange): Should be 0.1-2ms

### 2. **Stage Latency Percentiles**
- P50, P90, P99 for each stage
- Helps identify performance degradation
- Watch for P99 spikes

### 3. **Throughput per Stage (TPS)**
- Transactions per second completing each stage
- Stage 1 and Stage 3 should be roughly equal
- If Stage 1 > Stage 3, you have a growing backlog

### 4. **Stage 2 Duration Distribution (Heatmap)**
- Visual distribution of worker execution times
- Most transactions should cluster in predictable ranges
- Look for outliers

### 5. **Maximum Stage Duration**
- Tracks the slowest transaction in each window
- Good for spotting worst-case performance
- Thresholds: Green < 1s, Yellow < 10s, Red > 10s

### 6. **Transaction Count per Stage**
- Total transactions processed per stage in 5-minute windows
- All stages should have similar counts

### 7. **Throughput Backlog**
- Difference between Stage 1 and Stage 3 throughput
- Positive value = transactions piling up
- Should stay near zero
- Alert if > 10 TPS for extended periods

---

## PromQL Queries Reference

All queries use the `sov_sequencer_parallel_tx_stage` metric with these fields:
- `duration_us`: Duration in microseconds (converted to ms in queries)
- Tags: `stage` (1, 2, or 3), `tx_hash`

### Average Duration per Stage
```promql
avg by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000
```

### P99 Latency per Stage
```promql
histogram_quantile(0.99,
  sum by (stage, le) (
    rate(sov_sequencer_parallel_tx_stage_duration_us_bucket[5m])
  )
) / 1000
```

### Throughput (TPS) per Stage
```promql
sum by (stage) (rate(sov_sequencer_parallel_tx_stage_duration_us_count[1m]))
```

### Backlog Indicator
```promql
sum(rate(sov_sequencer_parallel_tx_stage_duration_us_count{stage="1"}[1m]))
-
sum(rate(sov_sequencer_parallel_tx_stage_duration_us_count{stage="3"}[1m]))
```

### Max Duration per Stage
```promql
max by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000
```

---

## Troubleshooting

### No data showing in Grafana?

1. **Check if Telegraf is receiving metrics:**
   ```bash
   curl http://localhost:9273/metrics | grep sov_sequencer_parallel_tx_stage
   ```

2. **Check if Prometheus is scraping Telegraf:**
   - Open http://localhost:9090
   - Go to Status → Targets
   - Ensure `telegraf` target is "UP"

3. **Check if metrics are in Prometheus:**
   ```bash
   curl 'http://localhost:9090/api/v1/query?query=sov_sequencer_parallel_tx_stage_duration_us'
   ```

4. **Ensure your sequencer is sending metrics:**
   - Check that your sequencer is configured to send metrics to `localhost:8094` (UDP)
   - Look for logs mentioning "STAGE 1", "STAGE 2", "STAGE 3"

### Dashboard shows "No data"?

- Wait for some transactions to be processed
- Reduce the time range in Grafana (top right) to "Last 5 minutes"
- Check if any parallel transactions are being processed (only midnight privacy txs use parallel executor)

### Metrics format issues?

If using Telegraf's histogram aggregation, you might need to adjust the queries. The current setup assumes Telegraf is passing through raw metrics to Prometheus without aggregation.

---

## Stopping the Stack

```bash
docker compose -f monitoring/docker-compose.yml down
```

To also remove data volumes:
```bash
docker compose -f monitoring/docker-compose.yml down -v
```

---

## Advanced Configuration

### Change Grafana Admin Password

Edit `monitoring/docker-compose.yml`:
```yaml
environment:
  - GF_SECURITY_ADMIN_PASSWORD=your_secure_password
```

### Increase Prometheus Retention

Edit `monitoring/docker-compose.yml`:
```yaml
command:
  - '--storage.tsdb.retention.time=90d'  # Change from 30d
```

### Add More Scrape Targets

Edit `monitoring/prometheus.yml`:
```yaml
scrape_configs:
  - job_name: 'telegraf'
    static_configs:
      - targets: ['telegraf:9273']
  - job_name: 'your-service'
    static_configs:
      - targets: ['your-service:9090']
```

Then restart:
```bash
docker compose -f monitoring/docker-compose.yml restart prometheus
```

---

## Setting Up Alerts

### In Grafana (Recommended)

1. Open the dashboard
2. Click on a panel title → Edit
3. Go to the "Alert" tab
4. Click "Create alert rule from this panel"
5. Configure thresholds and notification channels

### Example Alert Rules

**Stage 3 Taking Too Long:**
- Condition: `P99 > 10ms` for 5 minutes
- Action: Notify on Slack/Email

**Backlog Building Up:**
- Condition: `Stage1 TPS - Stage3 TPS > 10` for 5 minutes
- Action: Page on-call engineer

**No Transactions Processed:**
- Condition: `sum(rate(...[5m])) == 0` for 10 minutes
- Action: Check if sequencer is stuck

---

## Performance Tips

1. **Don't over-query**: The default 5s refresh is fine. Avoid < 1s refresh rates.

2. **Use appropriate time ranges**: For real-time monitoring, 15m-1h is sufficient.

3. **Archive old data**: Prometheus will automatically clean up based on retention settings.

4. **Dashboard variables**: You can add variables to filter by specific transaction hashes or stages.

---

## Next Steps

After the stack is running:

1. Run some transactions through your sequencer
2. Watch the metrics populate in real-time
3. Use the insights to optimize `num_parallel_tx_workers` in your sequencer config
4. Set up alerts for critical thresholds
5. Export and share the dashboard with your team
