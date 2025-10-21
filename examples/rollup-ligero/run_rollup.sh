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
    export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/bins/macos/bin/webgpu_verifier"
    BINS_DIR="$WORKSPACE_ROOT/crates/adapters/ligero/bins/macos"
elif [[ "$OSTYPE" == "linux-gnu"* ]]; then
    # Linux
    export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/bins/linux-amd64/bin/webgpu_verifier"
    BINS_DIR="$WORKSPACE_ROOT/crates/adapters/ligero/bins/linux-amd64"
else
    # Fallback to guest bins for other platforms
    export LIGERO_VERIFIER_BIN="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/webgpu_verifier"
    BINS_DIR="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins"
fi

# Set Ligero verification environment variables
# NOTE: LIGERO_PROGRAM_PATH is now optional - the verifier will auto-discover the correct program
# based on the code commitment (method_id) in the proof. This allows supporting both:
#   - midnight-privacy (note_spend_guest.wasm)
#   - value-setter-zk (value_validator.wasm)
# 
# If you want to force a specific program, uncomment one of these:
# export LIGERO_PROGRAM_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"
# export LIGERO_PROGRAM_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/value_validator.wasm"

export LIGERO_SHADER_PATH="$BINS_DIR/shader"
export LIGERO_PACKING=8192  # Must match the packing used during proof generation

# Verify files exist
if [ ! -f "$LIGERO_VERIFIER_BIN" ]; then
    echo "❌ Error: webgpu_verifier not found at: $LIGERO_VERIFIER_BIN"
    echo "   Run 'cd crates/adapters/ligero/guest && ./build.sh' to build it"
    exit 1
fi

# Check that at least one guest program exists
PROGRAMS_DIR="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs"
if [ ! -f "$PROGRAMS_DIR/note_spend_guest.wasm" ] && [ ! -f "$PROGRAMS_DIR/value_validator.wasm" ]; then
    echo "❌ Error: No guest programs found in $PROGRAMS_DIR"
    echo "   Build them with:"
    echo "   cd crates/adapters/ligero/guest/note-spend-guest"
    echo "   cargo build --release --target wasm32-unknown-unknown"
    echo "   cp target/wasm32-unknown-unknown/release/note_spend_guest.wasm ../bins/programs/"
    exit 1
fi

if [ ! -d "$LIGERO_SHADER_PATH" ]; then
    echo "❌ Error: shader directory not found at: $LIGERO_SHADER_PATH"
    echo "   Run 'cd crates/adapters/ligero/guest && ./build.sh' to build it"
    exit 1
fi

echo "✓ Ligero verification configuration:"
echo "  LIGERO_VERIFIER_BIN=$LIGERO_VERIFIER_BIN"
if [ -n "$LIGERO_PROGRAM_PATH" ]; then
    echo "  LIGERO_PROGRAM_PATH=$LIGERO_PROGRAM_PATH"
else
    echo "  LIGERO_PROGRAM_PATH=<auto-discovery enabled>"
    echo "  Available programs: $(ls -1 $PROGRAMS_DIR/*.wasm 2>/dev/null | xargs -n1 basename | tr '\n' ' ')"
fi
echo "  LIGERO_SHADER_PATH=$LIGERO_SHADER_PATH"
echo "  LIGERO_PACKING=$LIGERO_PACKING"
echo ""

# Optional: Skip verification for testing
if [ -n "$SKIP_VERIFICATION" ]; then
    export LIGERO_SKIP_VERIFICATION=1
    echo "⚠️  LIGERO_SKIP_VERIFICATION is set - proofs will NOT be verified!"
    echo ""
fi

# Build ligero rollup
echo "Building ligero rollup..."
cd "$WORKSPACE_ROOT"
cargo build --release -p sov-rollup-ligero

echo ""
echo "🚀 Starting ligero rollup..."
echo ""

# Run the ligero rollup from examples/rollup-ligero directory
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

# Create demo_data directory if it doesn't exist (required for SQLite DB)
mkdir -p demo_data

# export SOV_WORKER_TX_DB_CONNECTION_STRING="sqlite://demo_data/da.sqlite?mode=rwc"
export SOV_WORKER_TX_DB_CONNECTION_STRING="sqlite://$WORKSPACE_ROOT/examples/rollup-ligero/demo_data/da.sqlite?mode=rwc"

exec "$WORKSPACE_ROOT/target/release/sov-rollup-ligero"
