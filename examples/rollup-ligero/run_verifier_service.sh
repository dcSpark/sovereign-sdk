#!/bin/bash
set -e

# Script to run the proof verifier service with proper Ligero configuration
# This sets all required environment variables for Ligero proof verification

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Set Ligero verification environment variables
NOTE_SPEND_WASM="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"
VALUE_VALIDATOR_WASM="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/value_validator.wasm"

# The proof-verifier handles BOTH value-setter-zk and midnight-privacy. The Ligero adapter can
# auto-select the correct program based on the expected code commitment, but it still requires
# LIGERO_PROGRAM_PATH to be set. Default it to the Midnight program to match the common case.
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-$NOTE_SPEND_WASM}"
export LIGERO_PACKING=8192  # Must match the packing used during proof generation

if [ ! -f "$LIGERO_PROGRAM_PATH" ]; then
    echo "❌ Error: Ligero program not found at: $LIGERO_PROGRAM_PATH"
    echo "   Expected a WASM program (e.g. note_spend_guest.wasm)."
    echo "   Run 'cd crates/adapters/ligero/guest && ./build.sh' to build it."
    exit 1
fi

echo "✓ Ligero verification configuration:"
echo "  LIGERO_PROGRAM_PATH=$LIGERO_PROGRAM_PATH"
echo "  LIGERO_PACKING=$LIGERO_PACKING"
echo ""

# Optional overrides for the verifier service.
# If unset, the service will auto-compute method IDs from the WASM files on startup.
METHOD_ID="${METHOD_ID:-}"
MIDNIGHT_METHOD_ID="${MIDNIGHT_METHOD_ID:-}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:8080}"
NODE_RPC_URL="${NODE_RPC_URL:-http://127.0.0.1:12346}"
SIGNING_KEY_PATH="${SIGNING_KEY_PATH:-$WORKSPACE_ROOT/examples/test-data/keys/token_deployer_private_key.json}"
CHAIN_ID="${CHAIN_ID:-4321}"
LOG_LEVEL="${LOG_LEVEL:-info}"
MAX_CONCURRENT="${MAX_CONCURRENT:-10}"
ROLLUP_CONFIG_PATH="${ROLLUP_CONFIG_PATH:-$SCRIPT_DIR/rollup_config.toml}"

# Optional: Skip verification for testing
if [ -n "$SKIP_VERIFICATION" ]; then
    export LIGERO_SKIP_VERIFICATION=1
fi
if [ -n "$LIGERO_SKIP_VERIFICATION" ]; then
    echo "⚠️  LIGERO_SKIP_VERIFICATION is set - proofs will NOT be verified!"
    echo ""
fi

# Optional: Defer sequencer submission (queued mode)
DEFER_FLAG=""
if [ -n "$DEFER_SEQUENCER_SUBMISSION" ]; then
    # Lowercase in a POSIX-compatible way (bash 3 compatible)
    DSS_LC=$(printf "%s" "$DEFER_SEQUENCER_SUBMISSION" | tr '[:upper:]' '[:lower:]')
    case "$DSS_LC" in
        1|true|yes|on)
            DEFER_FLAG="--defer-submission"
            ;;
    esac
fi

# Build the verifier service
echo "Building proof verifier service..."
cd "$WORKSPACE_ROOT"
cargo build --release -p sov-proof-verifier-service

echo ""
echo "🚀 Starting proof verifier service..."
echo "   Bind address: $BIND_ADDR"
echo "   Node RPC: $NODE_RPC_URL"
echo "   Log level: $LOG_LEVEL"
echo "   Max concurrent: $MAX_CONCURRENT"
echo "   Rollup config: $ROLLUP_CONFIG_PATH"
echo ""

# Run the verifier service
METHOD_ID_ARGS=()
if [ -n "$METHOD_ID" ]; then
    METHOD_ID_ARGS+=(--method-id "$METHOD_ID")
fi
if [ -n "$MIDNIGHT_METHOD_ID" ]; then
    METHOD_ID_ARGS+=(--midnight-method-id "$MIDNIGHT_METHOD_ID")
fi

# Best-effort: warn if optional value-setter WASM isn't present (midnight still works).
if [ ! -f "$VALUE_VALIDATOR_WASM" ]; then
    echo "⚠️  Warning: value_validator.wasm not found at: $VALUE_VALIDATOR_WASM"
    echo "   The /value-setter-zk endpoint may be unavailable."
    echo ""
fi

exec "$WORKSPACE_ROOT/target/release/proof-verifier" \
    "${METHOD_ID_ARGS[@]}" \
    --bind "$BIND_ADDR" \
    --node-rpc-url "$NODE_RPC_URL" \
    --signing-key-path "$SIGNING_KEY_PATH" \
    --chain-id "$CHAIN_ID" \
    --log-level "$LOG_LEVEL" \
    --max-concurrent "$MAX_CONCURRENT" \
    --rollup-config-path "$ROLLUP_CONFIG_PATH" \
    $DEFER_FLAG
