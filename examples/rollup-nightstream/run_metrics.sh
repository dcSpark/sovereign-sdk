#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${SERVICE_TARGET_DIR:-release}"
BIN="$WORKSPACE_ROOT/target/$TARGET_DIR/sov-metrics-api"

BIND_ADDR="${METRICS_API_BIND:-0.0.0.0:13200}"
export RUST_LOG="${RUST_LOG:-info}"

export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-postgresql://admin:1234@localhost:5432/da}"
export INDEXER_DB_CONNECTION_STRING="${INDEXER_DB_CONNECTION_STRING:-${INDEX_DB:-postgresql://admin:1234@localhost:5432/indexer}}"
export LEDGER_API_URL="${LEDGER_API_URL:-${ROLLUP_RPC_URL:-http://127.0.0.1:12346}}"
export TSINK_DATA_PATH="${TSINK_DATA_PATH:-$SCRIPT_DIR/tsink-data}"
export METRICS_API_BIND="$BIND_ADDR"

mkdir -p "$TSINK_DATA_PATH"

echo "Metrics API Service"
echo "  Bind address:  $BIND_ADDR"
echo "  DA connection: $DA_CONNECTION_STRING"
echo "  Indexer DB:    $INDEXER_DB_CONNECTION_STRING"
echo "  Ledger API:    $LEDGER_API_URL"

if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build ${TARGET_DIR/release/--release }-p sov-metrics-api"
  exit 1
fi

cd "$WORKSPACE_ROOT/examples/rollup-nightstream"
exec "$BIN"
