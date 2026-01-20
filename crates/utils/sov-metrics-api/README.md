# sov-metrics-api

Small HTTP API that exposes raw counter metrics from the verifier worker DB
(`worker_verified_transactions`) and the indexer DB.

## Configuration

- `DA_CONNECTION_STRING` (required): sqlite or postgres connection string.
  - Example (sqlite): `sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc`
  - Example (postgres): `postgres://user:pass@localhost:5432/da_db`
- `INDEXER_DB_CONNECTION_STRING` (required): indexer DB connection string for `midnight_transfer`.
  - If unset, `INDEX_DB` is used as a fallback.
- `METRICS_API_BIND` (optional): listen address, default `0.0.0.0:13200`

## Behavior

- Collectors store raw counters (monotonic totals), not derived rates.
- `total-transactions` samples every 5 seconds (completed tx counter).
- `failed-transactions-rate` samples every 5 seconds (completed + rejected counters).
- `average-transaction-size` samples every 1 second (transfer amount + count counters).
- `token-value-spent` samples every 5 seconds (transfer amount counter).
- The API derives TPS, rates, and averages from counter deltas at query time.
- Derived endpoints accept `window_seconds` to compute deltas over a custom window; otherwise the
  last two samples are used.

## Architecture

- Collectors implement a `MetricCollector` trait and encapsulate data retrieval.
- `MetricsManager` schedules collectors and writes samples into `MetricsStore`.
- `MetricsStore` keeps per-metric ring buffers in memory.
- The API reads metric snapshots from the store.

## Endpoints

- `GET /health`
  - Returns `{ "status": "ok" }`
- `GET /tps`
  - Returns `{ tps, delta_transactions, delta_ms, latest_total }` derived from counters.
  - Optional: `?window_seconds=60`.
- `GET /total-transactions`
  - Returns `{ series, retention_seconds }` with raw counters.
- `GET /failed-transactions-rate`
  - Returns `{ series, rate_percent, delta_rejected, delta_completed, delta_ms, retention_seconds }`.
  - Optional: `?window_seconds=60`.
- `GET /average-transaction-size`
  - Returns `{ series, average_amount, delta_amount, delta_transactions, delta_ms, retention_seconds }`.
  - Optional: `?window_seconds=60`.
- `GET /token-value-spent`
  - Returns `{ series, delta_amount, delta_ms, retention_seconds }`.
  - Optional: `?window_seconds=60`.
- Derived fields (`tps`, `rate_percent`, `average_amount`, `delta_*`) are computed from the last
  two counter samples unless `window_seconds` is provided; the `series` fields always carry raw
  counters.
- Swagger UI: `GET /swagger-ui/`
- OpenAPI JSON: `GET /api-doc/openapi.json`

## Run

```bash
DA_CONNECTION_STRING=sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc \
INDEXER_DB_CONNECTION_STRING=sqlite://wallet_index.sqlite?mode=rwc \
METRICS_API_BIND=0.0.0.0:13200 \
cargo run -p sov-metrics-api
```
