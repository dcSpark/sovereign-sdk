# sov-metrics-api

Small HTTP API that exposes raw counter metrics from the verifier worker DB
(`worker_verified_transactions`) and the indexer DB.

## Configuration

- `DA_CONNECTION_STRING` (required): sqlite or postgres connection string.
  - Example (sqlite): `sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc`
  - Example (postgres): `postgres://user:pass@localhost:5432/da_db`
- `INDEXER_DB_CONNECTION_STRING` (required): indexer DB connection string for `midnight_transfer`.
  - If unset, `INDEX_DB` is used as a fallback.
- `TSINK_DATA_PATH` (required): directory path for tsink on-disk storage.
- `METRICS_API_BIND` (optional): listen address, default `0.0.0.0:13200`

## Behavior

- Collectors store raw counters (monotonic totals), not derived rates.
- `total-transactions` samples every 5 seconds (completed tx counter).
- `failed-transactions-rate` samples every 5 seconds (total + failed counters).
- `average-transaction-size` is derived from `token-value-spent` counters (transfer amount + count).
- `transaction-size` stores individual transfer amounts (used for median calculations).
- `token-value-spent` samples every 5 seconds (transfer amount counter), retained for 24 hours.
- `total-tokens-economy` samples every 30 seconds (sum of deposit amounts).
- The API derives TPS, rates, and averages from counter deltas at query time.
- Derived endpoints accept `window_seconds` to compute deltas over a custom window; otherwise the
  last two samples are used.

## Architecture

- Collectors implement a `MetricCollector` trait and encapsulate data retrieval.
- `MetricsManager` schedules collectors and writes samples into `MetricsStore`.
- `MetricsStore` uses the embedded `tsink` time-series engine with millisecond precision and
  a 1-day retention window.
- The API reads metric snapshots from the store.

## Endpoints

- `GET /health`
  - Returns `{ "status": "ok" }`
- `GET /tps`
  - Returns `{ tps, delta_transactions, delta_ms, latest_total }` derived from counters.
  - Optional: `?window_seconds=60`.
- `GET /total-transactions`
  - Returns `{ total_transactions, as_of_ms }` with the latest cumulative value.
- `GET /failed-transactions-rate`
  - Returns `{ rate_percent, failed_transactions, total_transactions, delta_ms, retention_seconds }`.
  - Optional: `?window_seconds=60`.
- `GET /average-transaction-size`
  - Returns `{ average_amount, delta_amount, delta_transactions, delta_ms, retention_seconds }`.
  - Defaults to 24 hours; optional: `?window_seconds=86400`.
  - Computed as `token-value-spent / number of transactions` over the same window.
- `GET /median-transaction-size`
  - Returns `{ median_amount }`.
  - Defaults to 24 hours; optional: `?window_seconds=86400`.
- `GET /token-value-spent`
  - Returns `{ value_spent }`.
  - Defaults to 24 hours; optional: `?window_seconds=86400`.
- `GET /token-velocity`
  - Returns `{ token_velocity, value_spent, total_tokens }`.
  - Defaults to 24 hours; optional: `?window_seconds=86400`.
  - Optional range override: `?from_ms=...&to_ms=...` (milliseconds since epoch).
  - `total_tokens` is the average supply over the range, derived from deposit totals.
- Derived fields (`tps`, `rate_percent`, `average_amount`, `median_amount`, `value_spent`,
  `token_velocity`, `delta_*`) are computed from the last two counter samples unless
  `window_seconds` is provided.
- Swagger UI: `GET /swagger-ui/`
- OpenAPI JSON: `GET /api-doc/openapi.json`

## Run

```bash
DA_CONNECTION_STRING=sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc \
INDEXER_DB_CONNECTION_STRING=sqlite://wallet_index.sqlite?mode=rwc \
TSINK_DATA_PATH=./tsink-data \
METRICS_API_BIND=0.0.0.0:13200 \
cargo run -p sov-metrics-api
```
