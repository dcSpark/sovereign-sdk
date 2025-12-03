#!/bin/bash
set -e

# Script to run the proof verifier service with samply profiling
# This sets all required environment variables for Ligero proof verification
# and wraps the execution with samply for performance profiling

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Profile output configuration
PROFILE_DIR="$SCRIPT_DIR/profiles"
mkdir -p "$PROFILE_DIR"
PROFILE_FILE="$PROFILE_DIR/rollup-profile-$(date +%Y%m%d-%H%M%S).json"

# Use the 'profiling' cargo profile which has debug=true and strip=false
CARGO_PROFILE="profiling"

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

# Check if samply is installed
if ! command -v samply &> /dev/null; then
    echo "❌ Error: samply is not installed"
    echo "   Install it with: cargo install samply"
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

# Build ligero rollup with the 'profiling' profile (release + debug symbols)
echo "Building ligero rollup with 'profiling' profile (release + debug symbols)..."
cd "$WORKSPACE_ROOT"
cargo build --profile $CARGO_PROFILE -p sov-rollup-ligero

echo ""
echo "🔬 Starting ligero rollup with samply profiler..."
echo "   Profile will be saved to: $PROFILE_FILE"
echo "   Press Ctrl+C to stop profiling"
echo ""

# Set RUST_LOG to info level to suppress debug logs
export RUST_LOG="info"

# Run the ligero rollup from examples/rollup-ligero directory
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

# Create demo_data directory if it doesn't exist (required for SQLite DB)
mkdir -p demo_data

# The binary is in target/$CARGO_PROFILE/ directory
BINARY_PATH="$WORKSPACE_ROOT/target/$CARGO_PROFILE/sov-rollup-ligero"

# Run with samply profiler, saving to a specific file
# --save-only: Save profile to file without opening browser
# -o: Output file path
# Note: On macOS, you may need to run with sudo for dtrace permissions
echo "Running: samply record --save-only -o $PROFILE_FILE $BINARY_PATH"
samply record --save-only -o "$PROFILE_FILE" "$BINARY_PATH" 2>&1

echo ""
echo "✅ Profile saved to: $PROFILE_FILE"
echo "   To view: samply load \"$PROFILE_FILE\""
echo "   Or open https://profiler.firefox.com and load the file manually"

