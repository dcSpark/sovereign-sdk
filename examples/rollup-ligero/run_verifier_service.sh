#!/bin/bash
set -e

# Script to run the proof verifier service with proper Ligero configuration
# This sets all required environment variables for Ligero proof verification

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Set Ligero verification environment variables
export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/webgpu_verifier"
export LIGERO_PROGRAM_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/value_validator.wasm"
export LIGERO_SHADER_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/shader"
export LIGERO_PACKING=8192  # Must match the packing used during proof generation

# Verify files exist
if [ ! -f "$LIGERO_VERIFIER_BIN" ]; then
    echo "❌ Error: webgpu_verifier not found at: $LIGERO_VERIFIER_BIN"
    echo "   Run 'cd crates/adapters/ligero/guest && ./build.sh' to build it"
    exit 1
fi

if [ ! -f "$LIGERO_PROGRAM_PATH" ]; then
    echo "❌ Error: value_validator.wasm not found at: $LIGERO_PROGRAM_PATH"
    echo "   Run 'cd crates/adapters/ligero/guest && ./build.sh' to build it"
    exit 1
fi

if [ ! -f "$LIGERO_SHADER_PATH" ]; then
    echo "❌ Error: shader not found at: $LIGERO_SHADER_PATH"
    echo "   Run 'cd crates/adapters/ligero/guest && ./build.sh' to build it"
    exit 1
fi

echo "✓ Ligero verification configuration:"
echo "  LIGERO_VERIFIER_BIN=$LIGERO_VERIFIER_BIN"
echo "  LIGERO_PROGRAM_PATH=$LIGERO_PROGRAM_PATH"
echo "  LIGERO_SHADER_PATH=$LIGERO_SHADER_PATH"
echo "  LIGERO_PACKING=$LIGERO_PACKING"
echo ""

# Default values for the verifier service
METHOD_ID="${METHOD_ID:-0xff8a9b0d64b0781fbcb1810b375aeb24f8b374b584d58ed2a470cafc0c3856c5}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:8080}"
NODE_RPC_URL="${NODE_RPC_URL:-http://127.0.0.1:12346}"
SIGNING_KEY_PATH="${SIGNING_KEY_PATH:-$WORKSPACE_ROOT/examples/test-data/keys/token_deployer_private_key.json}"
CHAIN_ID="${CHAIN_ID:-4321}"
LOG_LEVEL="${LOG_LEVEL:-info}"
MAX_CONCURRENT="${MAX_CONCURRENT:-10}"

# Optional: Skip verification for testing
if [ -n "$SKIP_VERIFICATION" ]; then
    export LIGERO_SKIP_VERIFICATION=1
    echo "⚠️  LIGERO_SKIP_VERIFICATION is set - proofs will NOT be verified!"
    echo ""
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
echo ""

# Run the verifier service
exec "$WORKSPACE_ROOT/target/release/proof-verifier" \
    --method-id "$METHOD_ID" \
    --bind "$BIND_ADDR" \
    --node-rpc-url "$NODE_RPC_URL" \
    --signing-key-path "$SIGNING_KEY_PATH" \
    --chain-id "$CHAIN_ID" \
    --log-level "$LOG_LEVEL" \
    --max-concurrent "$MAX_CONCURRENT"

