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
export TSINK_DATA_PATH="${TSINK_DATA_PATH:-$SCRIPT_DIR/tsink-data}"
export METRICS_API_BIND="$BIND_ADDR"

# Ensure directories exist
mkdir -p "$(dirname "$SCRIPT_DIR/demo_data/da.sqlite")" 2>/dev/null || true
mkdir -p "$TSINK_DATA_PATH"

echo "========================================"
echo "Metrics API Service"
echo "========================================"
echo "Bind address:    $BIND_ADDR"
echo "DA connection:   $DA_CONNECTION_STRING"
echo "Indexer DB:      $INDEXER_DB_CONNECTION_STRING"
echo "Tsink data:      $TSINK_DATA_PATH"
echo "Log level:       ${RUST_LOG}"
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

cd "$WORKSPACE_ROOT/examples/rollup-ligero"
exec cargo run -p sov-metrics-api --release
