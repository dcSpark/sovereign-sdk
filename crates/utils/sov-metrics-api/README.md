# sov-metrics-api

Small HTTP API that exposes raw counter metrics from the verifier worker DB
(`worker_verified_transactions`) and the indexer DB.

## Configuration

- `DA_CONNECTION_STRING` (required): sqlite or postgres connection string.
  - Example (sqlite): `sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc`
  - Example (postgres): `postgres://user:pass@localhost:5432/da_db`
- `INDEXER_DB_CONNECTION_STRING` (required): indexer DB connection string for `midnight_transfer`.
  - If unset, `INDEX_DB` is used as a fallback.
- `LEDGER_API_URL` (optional): base URL for rollup ledger API.
  - If unset, falls back to `ROLLUP_RPC_URL`, then `NODE_API_URL`, then `http://127.0.0.1:12346`.
- `TSINK_DATA_PATH` (required): directory path for tsink on-disk storage.
- `TSINK_RETENTION_SECONDS` (optional): tsink retention window in seconds, default 432000 (5 days).
- `METRICS_API_BIND` (optional): listen address, default `0.0.0.0:13200`
- `SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS` (optional): max Postgres pool size for DA DB, default `10`.
- `SOV_METRICS_API_DA_POSTGRES_MIN_CONNECTIONS` (optional): min Postgres pool size for DA DB, default `0`.
- `SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS` (optional): max Postgres pool size for indexer DB, default `10`.
- `SOV_METRICS_API_INDEXER_POSTGRES_MIN_CONNECTIONS` (optional): min Postgres pool size for indexer DB, default `0`.
- `SOV_METRICS_API_POSTGRES_ACQUIRE_TIMEOUT_SECS` (optional): pool acquire timeout seconds, default `30`.
- `SOV_METRICS_API_POSTGRES_IDLE_TIMEOUT_SECS` (optional): pool idle timeout seconds, default `600`.
- `SOV_METRICS_API_POSTGRES_MAX_LIFETIME_SECS` (optional): pool max lifetime seconds, default `1800`.
- `SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ENABLED` (optional): enables automatic Postgres materialized-view refreshes, default `true`. Set to `false` as an emergency RDS load-shedding switch.
- `SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_INTERVAL_MULTIPLIER` (optional): multiplies each built-in refresh interval, default `1`.
- `SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_MIN_INTERVAL_SECS` (optional): lower bound for each automatic materialized-view refresh interval, default `60`. Set to `0` to use the built-in per-view intervals.

## Behavior

- Collectors store raw counters (monotonic totals), not derived rates.
- On Postgres, several expensive aggregate counters are served through materialized views. The API refreshes each view in the background, with a default minimum refresh interval of 60 seconds to avoid continuously scanning large RDS tables.
- `total-transactions` samples every 5 seconds (completed tx counter).
- `failed-transactions-rate` samples every 5 seconds (total + failed counters).
- `average-transaction-size` is derived from `token-value-spent` counters (transfer amount + count).
- `transaction-size` stores individual transfer amounts (used for median calculations).
- `token-value-spent` samples every 5 seconds (transfer amount counter).
- `total-tokens-economy` samples every 30 seconds (sum of deposit amounts).
- TPS and PeakTPS are fetched from `/ledger/tps/latest` and `/ledger/tps/{slotId}`.
- The API derives remaining rates and averages from counter deltas at query time.
- Some counter-derived endpoints accept `window_seconds` to compute deltas over a custom window;
  otherwise the last two samples are used.

## Architecture

- Collectors implement a `MetricCollector` trait and encapsulate data retrieval.
- `MetricsManager` schedules collectors and writes samples into `MetricsStore`.
- `MetricsStore` uses the embedded `tsink` time-series engine with millisecond precision and
  a configurable retention window shared by all metrics.
- The API reads metric snapshots from the store.

## Endpoints

- `GET /health`
  - Returns `{ "status": "ok" }`
- `GET /tps`
  - Returns `{ tps, delta_transactions, delta_ms, latest_total }` using slot TPS.
  - Optional: `?window_seconds=60`.
- `GET /total-transactions`
  - Returns `{ total_transactions, as_of_ms }` with the latest cumulative value.
- `GET /failed-transactions-rate`
  - Returns `{ rate_percent, failed_transactions, total_transactions, delta_ms, retention_seconds }`.
  - Optional: `?window_seconds=60`.
- `GET /average-transaction-size`
  - Returns `{ average_amount, delta_amount, delta_transactions, delta_ms, retention_seconds }`.
  - Fixed 24-hour window (custom `window_seconds` is not supported on this endpoint).
  - Computed as `token-value-spent / number of transactions` over the same window.
- `GET /median-transaction-size`
  - Returns `{ median_amount }`.
  - Fixed 24-hour window (custom `window_seconds` is not supported on this endpoint).
- `GET /token-value-spent`
  - Returns `{ value_spent }`.
  - Defaults to 24 hours; optional: `?window_seconds=86400`.
- `GET /token-velocity`
  - Returns `{ token_velocity, value_spent, total_tokens }`.
  - Defaults to 24 hours; optional: `?window_seconds=86400`.
  - Optional range override: `?from_ms=...&to_ms=...` (milliseconds since epoch).
  - `total_tokens` is the average supply over the range, derived from deposit totals.
- Counter-derived fields (`rate_percent`, `value_spent`, `token_velocity`, `delta_*`) are
  computed from the last two counter samples unless `window_seconds` is provided.
- Historic endpoints return time-series samples and accept optional `from_ms`/`to_ms` query
  parameters (milliseconds since epoch). When provided, both must be set.
- Swagger UI: `GET /swagger-ui/`
- OpenAPI JSON: `GET /api-doc/openapi.json`

### Historic endpoints

- `GET /total-transactions/historic`
  - Returns `{ name, interval_secs, latest, samples }` for cumulative totals.
  - Optional: `?from_ms=...&to_ms=...`.
- `GET /tps/historic`
  - Returns `{ name, interval_secs, latest, samples }` for derived TPS samples.
  - Optional: `?window_seconds=60&from_ms=...&to_ms=...`.
- `GET /failed-transactions-rate/historic`
  - Returns `{ name, interval_secs, latest, samples }` for derived failure-rate samples.
  - Optional: `?window_seconds=60&from_ms=...&to_ms=...`.
- `GET /average-transaction-size/historic`
  - Returns `{ name, interval_secs, latest, samples }` for average size samples.
  - Defaults to 24 hours; optional: `?window_seconds=86400&from_ms=...&to_ms=...`.
- `GET /median-transaction-size/historic`
  - Returns `{ name, bucket_seconds, latest, samples }` for bucketed medians.
  - Defaults to 24 hours; optional: `?window_seconds=86400&from_ms=...&to_ms=...`.
- `GET /token-value-spent/historic`
  - Returns `{ name, interval_secs, latest, samples }` for value-spent samples.
  - Defaults to 24 hours; optional: `?window_seconds=86400&from_ms=...&to_ms=...`.
- `GET /token-velocity/historic`
  - Returns `{ name, interval_secs, latest, samples }` for token velocity samples.
  - Defaults to 24 hours; optional: `?window_seconds=86400&from_ms=...&to_ms=...`.

## Run

```bash
DA_CONNECTION_STRING=sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc \
INDEXER_DB_CONNECTION_STRING=sqlite://wallet_index.sqlite?mode=rwc \
LEDGER_API_URL=http://127.0.0.1:12346 \
TSINK_DATA_PATH=./tsink-data \
METRICS_API_BIND=0.0.0.0:13200 \
cargo run -p sov-metrics-api
```
