#!/bin/bash
set -e

# Script to run the proof verifier service with proper Ligero configuration
# This sets all required environment variables for Ligero proof verification

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Detect platform and set appropriate binary paths
if [[ "$OSTYPE" == "darwin"* ]]; then
    # macOS
    export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/bins/macos-arm64/bin/webgpu_verifier"
elif [[ "$OSTYPE" == "linux-gnu"* ]]; then
    # Linux
    export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/bins/linux-amd64/bin/webgpu_verifier"
else
    # Fallback to guest bins for other platforms
    export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/webgpu_verifier"
fi

# Set Ligero verification environment variables
export LIGERO_PROGRAM_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/value_validator.wasm"
export LIGERO_SHADER_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/bins/shader"
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

if [ ! -d "$LIGERO_SHADER_PATH" ]; then
    echo "❌ Error: shader directory not found at: $LIGERO_SHADER_PATH"
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
METHOD_ID="${METHOD_ID:-0x698c44527e4fa3f934471015da3caa61da1f4e167107dbba9df71a6545396fb3}"
MIDNIGHT_METHOD_ID="${MIDNIGHT_METHOD_ID:-0xd898d7673a91c7f18fda48b9ec6af8cba58edc4103c9d8e9a7a365aa04b62050}"
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
exec "$WORKSPACE_ROOT/target/release/proof-verifier" \
    --method-id "$METHOD_ID" \
    --midnight-method-id "$MIDNIGHT_METHOD_ID" \
    --bind "$BIND_ADDR" \
    --node-rpc-url "$NODE_RPC_URL" \
    --signing-key-path "$SIGNING_KEY_PATH" \
    --chain-id "$CHAIN_ID" \
    --log-level "$LOG_LEVEL" \
    --max-concurrent "$MAX_CONCURRENT" \
    --rollup-config-path "$ROLLUP_CONFIG_PATH" \
    $DEFER_FLAG
