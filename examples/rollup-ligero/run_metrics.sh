#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# run_metrics.sh - Run the sov-metrics-api service
#
# This script runs the metrics API that exposes raw counter metrics from the
# verifier worker DB and the indexer DB.
#
# Endpoints:
#   GET  /health                    - Health check endpoint
#   GET  /tps                       - Transactions per second
#   GET  /total-transactions        - Total cumulative transactions
#   GET  /failed-transactions-rate  - Failed transaction rate percentage
#   GET  /average-transaction-size  - Average transaction size
#   GET  /median-transaction-size   - Median transaction size
#   GET  /token-value-spent         - Token value spent
#   GET  /token-velocity            - Token velocity metric
#   GET  /swagger-ui/               - Swagger UI
#   GET  /api-doc/openapi.json      - OpenAPI spec
#
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------

BIND_ADDR="${METRICS_API_BIND:-0.0.0.0:13200}"
export RUST_LOG="${RUST_LOG:-info}"

# Database connections (same as indexer/verifier)
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-sqlite://demo_data/da.sqlite?mode=rwc}"
export INDEXER_DB_CONNECTION_STRING="${INDEXER_DB_CONNECTION_STRING:-${INDEX_DB:-sqlite://demo_data/wallet_index.sqlite?mode=rwc}}"
export LEDGER_API_URL="${LEDGER_API_URL:-${ROLLUP_RPC_URL:-${NODE_API_URL:-http://127.0.0.1:12346}}}"
export TSINK_DATA_PATH="${TSINK_DATA_PATH:-$SCRIPT_DIR/tsink-data}"
export METRICS_API_BIND="$BIND_ADDR"
export SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS="${SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS:-10}"
export SOV_METRICS_API_DA_POSTGRES_MIN_CONNECTIONS="${SOV_METRICS_API_DA_POSTGRES_MIN_CONNECTIONS:-0}"
export SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS="${SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS:-10}"
export SOV_METRICS_API_INDEXER_POSTGRES_MIN_CONNECTIONS="${SOV_METRICS_API_INDEXER_POSTGRES_MIN_CONNECTIONS:-0}"
export SOV_METRICS_API_POSTGRES_ACQUIRE_TIMEOUT_SECS="${SOV_METRICS_API_POSTGRES_ACQUIRE_TIMEOUT_SECS:-30}"
export SOV_METRICS_API_POSTGRES_IDLE_TIMEOUT_SECS="${SOV_METRICS_API_POSTGRES_IDLE_TIMEOUT_SECS:-600}"
export SOV_METRICS_API_POSTGRES_MAX_LIFETIME_SECS="${SOV_METRICS_API_POSTGRES_MAX_LIFETIME_SECS:-1800}"
export SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ENABLED="${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ENABLED:-false}"
export SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ON_STARTUP="${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ON_STARTUP:-false}"
export SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_INTERVAL_MULTIPLIER="${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_INTERVAL_MULTIPLIER:-1}"
export SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_MIN_INTERVAL_SECS="${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_MIN_INTERVAL_SECS:-300}"
export SOV_METRICS_API_TRANSACTION_SIZE_COLLECTOR_ENABLED="${SOV_METRICS_API_TRANSACTION_SIZE_COLLECTOR_ENABLED:-false}"
export SOV_METRICS_API_TRANSACTION_SIZE_BACKFILL_ENABLED="${SOV_METRICS_API_TRANSACTION_SIZE_BACKFILL_ENABLED:-false}"

# Ensure directories exist
mkdir -p "$(dirname "$SCRIPT_DIR/demo_data/da.sqlite")" 2>/dev/null || true
mkdir -p "$TSINK_DATA_PATH"

echo "========================================"
echo "Metrics API Service"
echo "========================================"
echo "Bind address:    $BIND_ADDR"
echo "DA connection:   $DA_CONNECTION_STRING"
echo "Indexer DB:      $INDEXER_DB_CONNECTION_STRING"
echo "Ledger API URL:  $LEDGER_API_URL"
echo "Tsink data:      $TSINK_DATA_PATH"
echo "Log level:       ${RUST_LOG}"
echo "PG DA pool:      max=${SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS} min=${SOV_METRICS_API_DA_POSTGRES_MIN_CONNECTIONS}"
echo "PG IDX pool:     max=${SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS} min=${SOV_METRICS_API_INDEXER_POSTGRES_MIN_CONNECTIONS}"
echo "PG pool timing:  acquire=${SOV_METRICS_API_POSTGRES_ACQUIRE_TIMEOUT_SECS}s idle=${SOV_METRICS_API_POSTGRES_IDLE_TIMEOUT_SECS}s life=${SOV_METRICS_API_POSTGRES_MAX_LIFETIME_SECS}s"
echo "MV refresh:      enabled=${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ENABLED} startup=${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_ON_STARTUP} min_interval=${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_MIN_INTERVAL_SECS}s multiplier=${SOV_METRICS_API_MATERIALIZED_VIEW_REFRESH_INTERVAL_MULTIPLIER}"
echo "Tx size:         enabled=${SOV_METRICS_API_TRANSACTION_SIZE_COLLECTOR_ENABLED} backfill=${SOV_METRICS_API_TRANSACTION_SIZE_BACKFILL_ENABLED}"
echo ""
echo "🚀 Starting Metrics API service..."
echo "   Endpoints:"
echo "     GET  http://${BIND_ADDR}/health                    - Health check"
echo "     GET  http://${BIND_ADDR}/tps                       - TPS metric"
echo "     GET  http://${BIND_ADDR}/total-transactions        - Total transactions"
echo "     GET  http://${BIND_ADDR}/failed-transactions-rate  - Failed tx rate"
echo "     GET  http://${BIND_ADDR}/average-transaction-size  - Avg tx size"
echo "     GET  http://${BIND_ADDR}/median-transaction-size   - Median tx size"
echo "     GET  http://${BIND_ADDR}/token-value-spent         - Token value spent"
echo "     GET  http://${BIND_ADDR}/token-velocity            - Token velocity"
echo "     GET  http://${BIND_ADDR}/swagger-ui/               - Swagger UI"
echo ""

BIN="$WORKSPACE_ROOT/target/release/sov-metrics-api"
if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build --release -p sov-metrics-api"
  exit 1
fi

cd "$WORKSPACE_ROOT/examples/rollup-ligero"
exec "$BIN"
