# sov-metrics-api

Small HTTP API that reports TPS-style metrics from the verifier worker DB
(`worker_verified_transactions`).

## Configuration

- `DA_CONNECTION_STRING` (required): sqlite or postgres connection string.
  - Example (sqlite): `sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc`
  - Example (postgres): `postgres://user:pass@localhost:5432/da_db`
- `METRICS_API_BIND` (optional): listen address, default `0.0.0.0:13200`

## Behavior

- Every 5 seconds, the service queries completed transactions
  (`accepted` or `rejected`) from the last 5 seconds.
- It computes the peak TPS (max transactions in any 1-second slice within
  that window) and stores it in an in-memory ring buffer.
- The buffer keeps the most recent 300 seconds of samples (60 samples).

## Architecture

- Collectors implement a `MetricCollector` trait and encapsulate data retrieval.
- `MetricsManager` schedules collectors and writes samples into `MetricsStore`.
- `MetricsStore` keeps per-metric ring buffers in memory.
- The API reads metric snapshots from the store.

## Endpoints

- `GET /health`
  - Returns `{ "status": "ok" }`
- `GET /tps`
  - Returns `{ series, window_seconds, retention_seconds }` where `series` includes
    `latest`, `samples`, `interval_secs`, and `max_samples`.
- `GET /total-transactions`
  - Returns `{ series, retention_seconds }` where `series` includes `latest`, `samples`,
    `interval_secs`, and `max_samples`.
- Swagger UI: `GET /swagger-ui/`
- OpenAPI JSON: `GET /api-doc/openapi.json`

## Run

```bash
DA_CONNECTION_STRING=sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc \
METRICS_API_BIND=0.0.0.0:13200 \
cargo run -p sov-metrics-api
```
