#!/usr/bin/env bash
set -euo pipefail

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

BIN="$WORKSPACE_ROOT/target/release/oracle"
if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build --release -p oracle"
  exit 1
fi

echo "🚀 Starting Oracle..."
cd "$WORKSPACE_ROOT/crates/oracle"

export ORACLE_SIGNING_KEY_HEX="${ORACLE_SIGNING_KEY_HEX:-0x26515ce9a1569fd28569e82a6aef29049d7197a79913d27f871a6ade48563354}"
export TEE_ORACLE_PUBKEY_HEX="${TEE_ORACLE_PUBKEY_HEX:-0x2e7a268b5b68ef23fd64ebfcfcf3b41b6ab74643051dcac82d724dac2091cf4d}"
export ORACLE_DB_CONNECTION_STRING="${DA_CONNECTION_STRING:-}"
exec "$BIN"
