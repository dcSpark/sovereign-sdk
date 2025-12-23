#!/bin/bash
set -e

# Script to run the proof verifier service with proper Ligero configuration
# This sets all required environment variables for Ligero proof verification

# Parse arguments
MEMORY_PROFILE=0
ROLLUP_ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --memory-profile)
            MEMORY_PROFILE=1
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS] [-- <rollup-args>...]"
            echo ""
            echo "Options:"
            echo "  --memory-profile    Enable macOS memory profiling for Instruments.app"
            echo "                      - Enables MallocStackLogging for better stack traces"
            echo "                      - Prints PID for easy attachment in Instruments"
            echo "                      - Open Instruments → Allocations/Leaks → Attach to Process"
            echo ""
            echo "  --help, -h          Show this help message"
            echo ""
            echo "Pass-through rollup args:"
            echo "  Anything after '--' is forwarded to 'sov-rollup-ligero'."
            echo "  Example: $0 -- --stop-at-rollup-height 1300"
            echo ""
            echo "Environment variables:"
            echo "  SKIP_VERIFICATION           Skip Ligero proof verification"
            echo "  LIGERO_SKIP_VERIFICATION    Same as SKIP_VERIFICATION"
            echo "  DEFER_SEQUENCER_SUBMISSION  Defer sequencer submission"
            exit 0
            ;;
        --)
            shift
            # Forward all remaining args verbatim to the rollup binary
            while [[ $# -gt 0 ]]; do
                ROLLUP_ARGS+=("$1")
                shift
            done
            ;;
        *)
            # Treat unknown args as rollup args (so users can omit the '--' if they want).
            ROLLUP_ARGS+=("$1")
            shift
            ;;
    esac
done

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
# NOTE: LIGERO_PROGRAM_PATH is now optional - the verifier will auto-discover the correct program
# based on the code commitment (method_id) in the proof. This allows supporting both:
#   - midnight-privacy (note_spend_guest.wasm)
#   - value-setter-zk (value_validator.wasm)
# 
# If you want to force a specific program, uncomment one of these:
# export LIGERO_PROGRAM_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"
# export LIGERO_PROGRAM_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/value_validator.wasm"

export LIGERO_SHADER_PATH="$WORKSPACE_ROOT/crates/adapters/ligero/bins/shader"
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
cd "$WORKSPACE_ROOT"

if [ "$MEMORY_PROFILE" -eq 1 ]; then
    echo "Building ligero rollup with debug symbols for profiling..."
    # Build with full debug symbols and frame pointers for proper stack traces
    CARGO_PROFILE_RELEASE_DEBUG=2 \
    CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off \
    RUSTFLAGS="-C force-frame-pointers=yes" \
    cargo build --release -p sov-rollup-ligero
    
    echo "Generating dSYM for Instruments symbolication..."
    # Generate dSYM bundle that Instruments uses for symbol resolution
    dsymutil "$WORKSPACE_ROOT/target/release/sov-rollup-ligero" -o "$WORKSPACE_ROOT/target/release/sov-rollup-ligero.dSYM"
    echo "   ✓ dSYM generated at target/release/sov-rollup-ligero.dSYM"
else
    echo "Building ligero rollup..."
    cargo build --release -p sov-rollup-ligero
fi

echo ""
if [ "$MEMORY_PROFILE" -eq 1 ]; then
    echo "🚀 Starting ligero rollup with memory profiling..."
else
    echo "🚀 Starting ligero rollup..."
fi
echo ""

# Default to info level (allow overriding via env)
export RUST_LOG="${RUST_LOG:-info}"

# Run the ligero rollup from examples/rollup-ligero directory
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

# Create demo_data directory if it doesn't exist (required for SQLite DB)
mkdir -p demo_data

# Memory profiling setup (macOS only)
if [ "$MEMORY_PROFILE" -eq 1 ]; then
    if [[ "$OSTYPE" != "darwin"* ]]; then
        echo "❌ Error: Memory profiling is only supported on macOS"
        exit 1
    fi
    
    echo "🔬 Memory profiling enabled for Instruments.app!"
    echo ""
    
    # Codesign the binary with get-task-allow entitlement for Instruments attachment
    echo "   Codesigning binary for Instruments attachment..."
    codesign -s - -f --entitlements /dev/stdin "$WORKSPACE_ROOT/target/release/sov-rollup-ligero" << 'ENTITLEMENTS' 2>/dev/null
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.get-task-allow</key>
    <true/>
</dict>
</plist>
ENTITLEMENTS
    echo "   ✓ Binary codesigned with get-task-allow entitlement"
    echo ""
    
    echo "   ┌─────────────────────────────────────────────────────────────────┐"
    echo "   │  INSTRUMENTS.APP SETUP                                          │"
    echo "   ├─────────────────────────────────────────────────────────────────┤"
    echo "   │  1. Open Instruments.app (Cmd+Space → 'Instruments')            │"
    echo "   │  2. Choose a template:                                          │"
    echo "   │     • 'Allocations' - track memory allocations over time        │"
    echo "   │     • 'Leaks' - detect memory leaks                             │"
    echo "   │     • 'VM Tracker' - virtual memory regions                     │"
    echo "   │  3. Click the target dropdown (top left) → 'Attach to Process'  │"
    echo "   │  4. Select 'sov-rollup-ligero' from the list                    │"
    echo "   │  5. Click the red Record button to start profiling              │"
    echo "   └─────────────────────────────────────────────────────────────────┘"
    echo ""
    echo "   PID will be printed below once the process starts."
    echo ""
    
    # Enable malloc stack logging - gives Instruments better stack traces
    export MallocStackLogging=1
    export MallocStackLoggingNoCompact=1
fi

# Run without capturing output - ensures eprintln! and all stderr/stdout are shown
if [ "$MEMORY_PROFILE" -eq 1 ]; then
    # Run in background briefly to get PID, then wait
    "$WORKSPACE_ROOT/target/release/sov-rollup-ligero" "${ROLLUP_ARGS[@]}" 2>&1 &
    ROLLUP_PID=$!
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "   📍 Process started with PID: $ROLLUP_PID"
    echo "   🔗 In Instruments: Attach to Process → sov-rollup-ligero ($ROLLUP_PID)"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    wait $ROLLUP_PID
else
    exec "$WORKSPACE_ROOT/target/release/sov-rollup-ligero" "${ROLLUP_ARGS[@]}" 2>&1
fi
