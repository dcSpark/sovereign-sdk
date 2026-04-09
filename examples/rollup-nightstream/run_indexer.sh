#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${SERVICE_TARGET_DIR:-release}"
BIN="$WORKSPACE_ROOT/target/$TARGET_DIR/sov-indexer"

export RUST_LOG="${RUST_LOG:-info}"
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-postgresql://admin:1234@localhost:5432/da}"

mkdir -p "$SCRIPT_DIR/demo_data"
export INDEX_DB="${INDEX_DB:-postgresql://admin:1234@localhost:5432/indexer}"

echo "Nightstream Indexer"
echo "  DA_CONNECTION_STRING=$DA_CONNECTION_STRING"
echo "  INDEX_DB=$INDEX_DB"

if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build ${TARGET_DIR/release/--release }-p sov-indexer"
  exit 1
fi

cd "$WORKSPACE_ROOT/examples/rollup-nightstream"
exec "$BIN"
