#!/usr/bin/env bash
set -euo pipefail

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

BIN="$WORKSPACE_ROOT/target/release/sov-indexer"
if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build --release -p sov-indexer"
  exit 1
fi

echo "🚀 Starting ligero indexer..."
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

export RUST_LOG="${RUST_LOG:-info}"
export SOV_INDEXER_SYNC_INTERVAL_MS="${SOV_INDEXER_SYNC_INTERVAL_MS:-1000}"
export SOV_INDEXER_RECONCILE_INTERVAL_SECS="${SOV_INDEXER_RECONCILE_INTERVAL_SECS:-60}"
export SOV_INDEXER_STARTUP_BACKFILLS_ENABLED="${SOV_INDEXER_STARTUP_BACKFILLS_ENABLED:-false}"

# DA_CONNECTION_STRING should match the [da].connection_string in rollup_config.toml
# Default to SQLite for local development, but respect environment override for PostgreSQL
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-sqlite://demo_data/da.sqlite?mode=rwc}"

mkdir -p demo_data
export INDEX_DB="${INDEX_DB:-sqlite://demo_data/wallet_index.sqlite?mode=rwc}"

echo "   DA_CONNECTION_STRING=$DA_CONNECTION_STRING"
echo "   INDEX_DB=$INDEX_DB"
echo "   SOV_INDEXER_SYNC_INTERVAL_MS=$SOV_INDEXER_SYNC_INTERVAL_MS"
echo "   SOV_INDEXER_RECONCILE_INTERVAL_SECS=$SOV_INDEXER_RECONCILE_INTERVAL_SECS"
echo "   SOV_INDEXER_STARTUP_BACKFILLS_ENABLED=$SOV_INDEXER_STARTUP_BACKFILLS_ENABLED"

exec "$BIN"
