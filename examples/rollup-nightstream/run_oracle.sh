#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

export ORACLE_SIGNING_KEY_HEX="${ORACLE_SIGNING_KEY_HEX:-0x26515ce9a1569fd28569e82a6aef29049d7197a79913d27f871a6ade48563354}"
export TEE_ORACLE_PUBKEY_HEX="${TEE_ORACLE_PUBKEY_HEX:-0x2e7a268b5b68ef23fd64ebfcfcf3b41b6ab74643051dcac82d724dac2091cf4d}"
export ORACLE_DB_CONNECTION_STRING="${DA_CONNECTION_STRING:-}"

echo "Oracle Service"
echo "  DB: $ORACLE_DB_CONNECTION_STRING"

cd "$WORKSPACE_ROOT/crates/oracle"
exec cargo run -p oracle --release
