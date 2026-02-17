#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

export RUST_LOG="${RUST_LOG:-info}"
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-postgresql://admin:1234@localhost:5432/da}"

mkdir -p "$SCRIPT_DIR/demo_data"
export INDEX_DB="${INDEX_DB:-postgresql://admin:1234@localhost:5432/indexer}"

echo "Nightstream Indexer"
echo "  DA_CONNECTION_STRING=$DA_CONNECTION_STRING"
echo "  INDEX_DB=$INDEX_DB"

cd "$WORKSPACE_ROOT/examples/rollup-nightstream"
exec cargo run -p sov-indexer --release
