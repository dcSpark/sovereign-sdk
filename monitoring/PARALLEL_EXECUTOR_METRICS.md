# Parallel Executor Performance Metrics Guide

## Overview

The parallel executor processes transactions through 3 distinct stages. This document explains each stage, the metrics collected, and how to visualize and interpret them in Grafana.

## The 3 Stages of Parallel Transaction Execution

### Stage 1: Sending to Parallel Executor
**Location:** `inner.rs:1871-1897`

**What happens:**
- Transaction is identified as a midnight privacy transaction
- Checks if parallel executor has capacity
- Sends transaction to the parallel worker pool via channel
- Registers HTTP waiter for result
- Increments pending parallel count

**Timing:** Typically < 100 microseconds (includes channel send operation)

**Metric collected:**
- Measurement: `sov_sequencer_parallel_tx_stage`
- Tags: `stage=1`, `tx_hash=<hash>`
- Field: `duration_us`

---

### Stage 2: Worker Execution
**Location:** `parallel_tx_executor.rs:359-433`

**What happens:**
- Worker receives transaction from channel
- Increments active worker counter
- Executes transaction using isolated executor instance
- Generates receipt and state changes
- Converts receipt to API effect format
- Sends completion message back to main sequencer
- Decrements active worker counter

**Timing:** Varies significantly based on transaction complexity
- Simple transactions: ~1-10 ms
- Complex midnight privacy transactions: 100-5000+ ms

**Metric collected:**
- Measurement: `sov_sequencer_parallel_tx_stage`
- Tags: `stage=2`, `tx_hash=<hash>`
- Field: `duration_us`

---

### Stage 3: Committing Result
**Location:** `inner.rs:1961-2130`

**What happens:**
- Receives parallel execution result
- Validates batch is still in progress
- Applies precomputed state changes to main checkpoint (FAST PATH)
- Processes receipt and assigns transaction numbers
- Updates batch size tracker
- Handles HTTP waiter notification (immediate or deferred based on `fast_ack_after_executor`)
- Enqueues side effects (DB writes, cache updates)
- Decrements pending parallel count
- Checks if batch should be closed

**Timing:** Typically 0.1-2 ms (FAST PATH avoids re-execution)

**Breakdown within Stage 3:**
- `commit_ms`: Applying state changes to checkpoint
- `process_receipt_ms`: Building AcceptedTx structure
- `batch_metrics_ms`: Updating batch tracker
- `waiter_bridge_ms`: Notifying HTTP caller
- `send_accept_ms`: Enqueueing side effects
- `close_batch_ms`: Checking/closing batch if needed

**Metric collected:**
- Measurement: `sov_sequencer_parallel_tx_stage`
- Tags: `stage=3`, `tx_hash=<hash>`
- Field: `duration_us`

---

## Grafana Queries (PromQL)

All queries use the `sov_sequencer_parallel_tx_stage` metric exposed by Telegraf to Prometheus.

### 1. Average Duration per Stage (Time Series)

Shows the average time spent in each stage over time.

```promql
avg by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000
```

**Visualization:** Time series graph
**Y-axis:** Duration (milliseconds)
**Legend:** Stage 1, Stage 2, Stage 3

---

### 2. P50, P90, P99 Latency per Stage (Time Series)

Shows latency percentiles for each stage to understand performance distribution.

```promql
# P99
histogram_quantile(0.99, sum by (stage, le) (rate(sov_sequencer_parallel_tx_stage_duration_us_bucket[5m]))) / 1000

# P90
histogram_quantile(0.90, sum by (stage, le) (rate(sov_sequencer_parallel_tx_stage_duration_us_bucket[5m]))) / 1000

# P50
histogram_quantile(0.50, sum by (stage, le) (rate(sov_sequencer_parallel_tx_stage_duration_us_bucket[5m]))) / 1000
```

**Visualization:** Time series graph with multiple series
**Y-axis:** Duration (milliseconds)
**Legend:** Stage 1 P50/P90/P99, Stage 2 P50/P90/P99, Stage 3 P50/P90/P99

---

### 3. Throughput per Stage (Transactions per Second)

Shows how many transactions complete each stage per second.

```promql
sum by (stage) (rate(sov_sequencer_parallel_tx_stage_duration_us_count[1m]))
```

**Visualization:** Time series graph
**Y-axis:** Transactions per second
**Legend:** Stage 1 TPS, Stage 2 TPS, Stage 3 TPS

---

### 4. Stage 2 Duration Distribution (Histogram)

Shows the distribution of Stage 2 (worker execution) times as a heatmap.

```promql
sum by (le) (rate(sov_sequencer_parallel_tx_stage_duration_us_bucket{stage="2"}[5m]))
```

**Visualization:** Heatmap or Histogram
**Format:** Set to "Heatmap" in Grafana
**X-axis:** Time
**Y-axis:** Duration buckets
**Color intensity:** Rate of transactions

---

### 5. Maximum Stage Duration

Shows the slowest transaction in each time window.

```promql
max by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000
```

**Visualization:** Time series graph
**Y-axis:** Duration (milliseconds)
**Legend:** Stage 1 Max, Stage 2 Max, Stage 3 Max

---

### 6. Transaction Count per Stage

Total transactions processed per stage over time.

```promql
sum by (stage) (increase(sov_sequencer_parallel_tx_stage_duration_us_count[5m]))
```

**Visualization:** Time series graph
**Y-axis:** Transaction count
**Legend:** Stage 1, Stage 2, Stage 3

---

### 7. Throughput Backlog (Warning Indicator)

Difference between Stage 1 input and Stage 3 output. Positive = growing backlog.

```promql
sum(rate(sov_sequencer_parallel_tx_stage_duration_us_count{stage="1"}[1m]))
-
sum(rate(sov_sequencer_parallel_tx_stage_duration_us_count{stage="3"}[1m]))
```

**Visualization:** Time series graph with threshold areas
**Y-axis:** TPS difference
**Thresholds:** Green < 5, Yellow < 10, Red > 10

---

### 8. Min/Avg/Max Duration Summary per Stage

Statistical summary showing range of durations.

```promql
# Min
min by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000

# Avg
avg by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000

# Max
max by (stage) (sov_sequencer_parallel_tx_stage_duration_us) / 1000
```

**Visualization:** Time series or stat panel
**Unit:** Milliseconds

---

## Interpreting the Dashboards

### What to Look For

#### 1. **Stage 1 Performance**
- **Expected:** < 100 microseconds
- **Concern if:** > 1 millisecond consistently
- **Indicates:** Channel congestion or system overload
- **Action:** Check `sov_sequencer_parallel_tx_metrics.tx_channel_size` metric

#### 2. **Stage 2 Performance**
- **Expected:** Highly variable (1ms to 5000ms depending on transaction)
- **Concern if:** Sudden increases in P99 or all transactions taking longer
- **Indicates:**
  - Worker pool saturation (check active_workers in logs)
  - Transaction complexity increase
  - System resource contention (CPU/memory)
- **Action:**
  - Check `ACTIVE_WORKERS` counter in logs
  - Consider increasing `num_parallel_tx_workers` in config
  - Monitor system CPU/memory usage

#### 3. **Stage 3 Performance**
- **Expected:** 0.1-2 milliseconds (FAST PATH)
- **Concern if:** > 5 milliseconds consistently
- **Indicates:**
  - Slow checkpoint application
  - Database/cache write backlog
  - Main sequencer bottleneck
- **Action:**
  - Check `commit_ms`, `send_accept_ms` breakdown in logs
  - Review `sov_rollup_preferred_sequencer_executor_event_sending` metrics
  - Check disk I/O if DB writes are slow

#### 4. **Throughput Comparison**
- **Expected:** Stage 1 TPS ≈ Stage 3 TPS (what goes in comes out)
- **Concern if:** Stage 1 TPS > Stage 3 TPS persistently
- **Indicates:** Backlog building up, transactions stuck in Stage 2
- **Action:** Increase worker pool size or investigate Stage 2 bottlenecks

#### 5. **End-to-End Latency**
- **Expected:** Approximately sum of Stage 1 + Stage 2 + Stage 3
- **Concern if:** Much larger than sum of stages
- **Indicates:** Queueing delays between stages
- **Action:** Review channel sizes and worker pool configuration

### Common Patterns

#### Pattern 1: Stage 2 Dominates
```
Stage 1: 0.05ms
Stage 2: 2000ms
Stage 3: 0.5ms
```
**Normal:** This is expected for complex privacy transactions. Stage 2 is the actual ZK proof verification/execution.

#### Pattern 2: Stage 3 Increasing
```
Stage 1: 0.05ms → 0.05ms (stable)
Stage 2: 1000ms → 1000ms (stable)
Stage 3: 0.5ms → 5ms (increasing)
```
**Problem:** Main sequencer becoming bottleneck, possibly due to:
- Growing state size slowing checkpoint operations
- Database write backlog
- Too many parallel transactions overwhelming Stage 3

**Solution:** May need to tune `max_concurrent_blobs` or reduce `num_parallel_tx_workers`

#### Pattern 3: All Stages Increasing Proportionally
```
Stage 1: 0.05ms → 0.5ms
Stage 2: 1000ms → 1200ms
Stage 3: 0.5ms → 1ms
```
**Problem:** System-wide resource exhaustion (CPU, memory, disk I/O)

**Solution:** Scale hardware or reduce load

### Alert Thresholds (Recommendations)

Create Grafana alerts for:

1. **Stage 1 P99 > 1ms** for 5 minutes
   - Severity: Warning
   - Action: Check channel congestion

2. **Stage 2 P99 > 10s** for 5 minutes
   - Severity: Warning
   - Action: Review transaction complexity or worker pool

3. **Stage 3 P99 > 10ms** for 5 minutes
   - Severity: Critical
   - Action: Main sequencer bottleneck, immediate investigation needed

4. **Throughput Stage1 - Stage3 > 10 TPS** for 5 minutes
   - Severity: Warning
   - Action: Backlog building up

## Example Grafana Dashboard JSON

Save this to a file and import into Grafana:

```json
{
  "dashboard": {
    "title": "Parallel Executor Performance",
    "panels": [
      {
        "title": "Stage Duration (P50, P90, P99)",
        "type": "timeseries",
        "gridPos": {"x": 0, "y": 0, "w": 12, "h": 8},
        "targets": [
          {
            "query": "from(bucket: \"telegraf\")\\n  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)\\n  |> filter(fn: (r) => r[\"_measurement\"] == \"sov_sequencer_parallel_tx_stage\")\\n  |> filter(fn: (r) => r[\"_field\"] == \"duration_us\")\\n  |> group(columns: [\"stage\"])\\n  |> aggregateWindow(every: v.windowPeriod, fn: (column, tables=<-) => tables |> quantile(q: 0.99, column: \"_value\"))\\n  |> map(fn: (r) => ({ r with _value: r._value / 1000.0 }))"
          }
        ]
      },
      {
        "title": "Throughput (TPS per Stage)",
        "type": "timeseries",
        "gridPos": {"x": 12, "y": 0, "w": 12, "h": 8},
        "targets": [
          {
            "query": "from(bucket: \"telegraf\")\\n  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)\\n  |> filter(fn: (r) => r[\"_measurement\"] == \"sov_sequencer_parallel_tx_stage\")\\n  |> filter(fn: (r) => r[\"_field\"] == \"duration_us\")\\n  |> group(columns: [\"stage\"])\\n  |> aggregateWindow(every: 1s, fn: count)"
          }
        ]
      },
      {
        "title": "Stage 2 Duration Distribution",
        "type": "histogram",
        "gridPos": {"x": 0, "y": 8, "w": 12, "h": 8},
        "targets": [
          {
            "query": "from(bucket: \"telegraf\")\\n  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)\\n  |> filter(fn: (r) => r[\"_measurement\"] == \"sov_sequencer_parallel_tx_stage\")\\n  |> filter(fn: (r) => r[\"_field\"] == \"duration_us\")\\n  |> filter(fn: (r) => r[\"stage\"] == \"2\")\\n  |> map(fn: (r) => ({ r with _value: r._value / 1000.0 }))"
          }
        ]
      },
      {
        "title": "Slowest Transactions (Stage 2)",
        "type": "table",
        "gridPos": {"x": 12, "y": 8, "w": 12, "h": 8},
        "targets": [
          {
            "query": "from(bucket: \"telegraf\")\\n  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)\\n  |> filter(fn: (r) => r[\"_measurement\"] == \"sov_sequencer_parallel_tx_stage\")\\n  |> filter(fn: (r) => r[\"_field\"] == \"duration_us\")\\n  |> filter(fn: (r) => r[\"stage\"] == \"2\")\\n  |> map(fn: (r) => ({ r with _value: r._value / 1000.0 }))\\n  |> sort(columns: [\"_value\"], desc: true)\\n  |> limit(n: 10)"
          }
        ]
      }
    ]
  }
}
```

## Summary

The 3-stage parallel executor provides detailed visibility into transaction processing:

1. **Stage 1** tracks submission overhead
2. **Stage 2** captures actual transaction execution time (the bulk of work)
3. **Stage 3** measures result integration back into the main sequencer

By monitoring these metrics, you can:
- Identify bottlenecks in the parallel execution pipeline
- Optimize worker pool sizing
- Detect system resource issues
- Understand transaction complexity patterns
- Set appropriate SLAs for transaction processing

The metrics are automatically exported to Telegraf and can be visualized in Grafana using the queries provided above.
