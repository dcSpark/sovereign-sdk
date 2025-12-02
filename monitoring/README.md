# Parallel Executor Monitoring

This directory (`monitoring/`) contains everything you need to monitor the performance of the parallel executor in real-time. Commands below assume you're running them from the repository root.

## Quick Start (TL;DR)

```bash
./monitoring/launch-monitoring.sh
```

Then open http://localhost:3000 (admin/admin) and go to the **Parallel Executor Performance** dashboard.

---

## What's Included

### 📊 Metrics Collection
- **Telegraf metrics** tracking 3 stages of parallel transaction execution:
  - **Stage 1**: Sending to parallel executor (< 0.1ms)
  - **Stage 2**: Worker execution (1-5000ms, varies by transaction complexity)
  - **Stage 3**: Committing result to main executor (0.1-2ms)

### 🎯 Pre-configured Stack
- **Docker Compose** setup with:
  - Telegraf (receives metrics from sequencer on UDP 8094)
  - Prometheus (scrapes Telegraf on port 9273)
  - Grafana (visualizes data on port 3000)

### 📈 Ready-to-Use Dashboard
- **7 visualization panels** showing:
  - Stage durations (avg, P50/P90/P99, max)
  - Throughput per stage (TPS)
  - Stage 2 distribution (heatmap)
  - Backlog indicator
  - Transaction counts

---

## Files Overview

| File | Purpose |
|------|---------|
| `launch-monitoring.sh` | One-command script to start everything |
| `docker-compose.yml` | Container orchestration config |
| `telegraf.conf` | Telegraf input/aggregation/output config |
| `prometheus.yml` | Prometheus scrape config |
| `grafana/` | Grafana provisioning (datasources + dashboards) |
| `GRAFANA_SETUP.md` | Detailed setup and troubleshooting guide |
| `PARALLEL_EXECUTOR_METRICS.md` | Deep dive into the 3 stages and PromQL queries |

---

## How It Works

```
Sequencer Process
     ↓ (UDP 8094, Influx line protocol)
Telegraf
     ↓ (creates histograms, exposes on :9273)
Prometheus
     ↓ (scrapes every 5s, stores time-series)
Grafana
     ↓ (queries Prometheus, renders dashboards)
Your Browser (http://localhost:3000)
```

---

## Usage

### Start the Stack
```bash
./monitoring/launch-monitoring.sh
# or
docker compose -f monitoring/docker-compose.yml up -d
```

### Access Grafana
1. Open http://localhost:3000
2. Login: `admin` / `admin`
3. Navigate: **Dashboards → Browse → Sequencer → Parallel Executor Performance**

### View Raw Metrics
- **Telegraf metrics**: http://localhost:9273/metrics
- **Prometheus UI**: http://localhost:9090
- **Prometheus query**: http://localhost:9090/graph?g0.expr=sov_sequencer_parallel_tx_stage_duration_us

### Stop the Stack
```bash
docker compose -f monitoring/docker-compose.yml down
```

---

## What You'll Learn

### Performance Insights
- **Which stage is the bottleneck?** If Stage 2 dominates, that's expected (actual ZK proof work). If Stage 3 grows, you have a main sequencer bottleneck.
- **Is there a backlog?** Check the "Throughput Backlog" panel. Positive = transactions piling up.
- **Worker pool sizing**: If Stage 2 P99 is high and you have CPU headroom, increase `num_parallel_tx_workers`.
- **System health**: Sudden increases in all stages = resource exhaustion.

### Optimization Opportunities
1. **Stage 1 slow?** Channel congestion, increase channel size
2. **Stage 2 slow?** Add more workers or upgrade CPU
3. **Stage 3 slow?** Main sequencer bottleneck, tune DB/cache or reduce parallel concurrency
4. **Growing backlog?** Workers can't keep up with input rate

---

## Troubleshooting

### No data in Grafana?
```bash
# Check if metrics are reaching Telegraf
curl http://localhost:9273/metrics | grep sov_sequencer_parallel_tx_stage

# Check if Prometheus is scraping
curl 'http://localhost:9090/api/v1/query?query=sov_sequencer_parallel_tx_stage_duration_us'

# Check container logs
docker compose -f monitoring/docker-compose.yml logs -f telegraf
docker compose -f monitoring/docker-compose.yml logs -f prometheus
docker compose -f monitoring/docker-compose.yml logs -f grafana
```

### Sequencer not sending metrics?
Ensure your sequencer is running and configured to send metrics to `localhost:8094` (UDP). Check for log lines with:
- `[STAGE 1]`, `[STAGE 2]`, `[STAGE 3]`
- `[PARALLEL]`

### Dashboard shows "No data"?
- Wait for transactions to be processed (only midnight privacy txs use parallel executor)
- Reduce time range to "Last 5 minutes" in Grafana
- Run some test transactions

---

## Advanced

### Custom Dashboards
- Copy the provided dashboard and modify
- Use the PromQL queries from `PARALLEL_EXECUTOR_METRICS.md`
- Save and share with your team

### Alerts
Set up Grafana alerts for:
- **Stage 3 P99 > 10ms** for 5 minutes (critical)
- **Backlog > 10 TPS** for 5 minutes (warning)
- **Stage 2 P99 > 10s** for 5 minutes (warning)

### Data Retention
Default: 30 days. To change, edit `monitoring/docker-compose.yml`:
```yaml
command:
  - '--storage.tsdb.retention.time=90d'
```

### Prometheus Federation
To scrape from multiple sequencers, add targets to `monitoring/prometheus.yml`:
```yaml
scrape_configs:
  - job_name: 'telegraf-node1'
    static_configs:
      - targets: ['node1:9273']
  - job_name: 'telegraf-node2'
    static_configs:
      - targets: ['node2:9273']
```

---

## Documentation

| Document | Contents |
|----------|----------|
| **GRAFANA_SETUP.md** | Step-by-step setup, troubleshooting, PromQL reference |
| **PARALLEL_EXECUTOR_METRICS.md** | Deep technical dive into the 3 stages, interpretation guide |

---

## Need Help?

1. Check `GRAFANA_SETUP.md` for detailed troubleshooting
2. Review `PARALLEL_EXECUTOR_METRICS.md` for understanding what metrics mean
3. Check container logs: `docker compose -f monitoring/docker-compose.yml logs -f`
4. Verify metrics pipeline: Sequencer → Telegraf → Prometheus → Grafana

---

## Summary

With this monitoring setup, you now have:
- ✅ Real-time visibility into parallel executor performance
- ✅ 7 pre-configured visualizations
- ✅ Ability to identify bottlenecks and optimize worker pool
- ✅ Historical data for capacity planning
- ✅ Foundation for setting up alerts and SLAs

**Start monitoring:** `./monitoring/launch-monitoring.sh` 🚀
