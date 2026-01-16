#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# run_prover.sh - Run the Ligero HTTP prover service
#
# This script runs the ligero-http-server binary from the ligero-prover git
# dependency, which provides HTTP endpoints for ZK proof generation/verification.
#
# Endpoints:
#   POST /prove  - Generate a proof for a given circuit
#   POST /verify - Verify an existing proof
#   GET  /health - Health check endpoint
#
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------

BIND_ADDR="${PROVER_BIND_ADDR:-0.0.0.0:1313}"
THREADS="${PROVER_THREADS:-}"
PROOF_OUTPUTS="${PROVER_PROOF_OUTPUTS:-$SCRIPT_DIR/proof_outputs}"
KEEP_PROOF_DIRS="${PROVER_KEEP_PROOF_DIRS:-}"
export RUST_LOG="${RUST_LOG:-info}"

# -----------------------------------------------------------------------------
# Find ligero-prover git checkout
# -----------------------------------------------------------------------------

# The ligero-runner crate is fetched as a git dependency - find its location
LIGERO_CHECKOUT=$(find ~/.cargo/git/checkouts/ligero-prover-* -maxdepth 1 -type d -name "a581a9c*" 2>/dev/null | head -1)

if [[ -z "$LIGERO_CHECKOUT" ]]; then
    echo "❌ Could not find ligero-prover git checkout."
    echo "   Make sure you've run 'cargo build' at least once to fetch dependencies."
    exit 1
fi

RUNNER_DIR="$LIGERO_CHECKOUT/utils/ligero-webgpu-runner"

if [[ ! -d "$RUNNER_DIR" ]]; then
    echo "❌ Could not find ligero-webgpu-runner at: $RUNNER_DIR"
    exit 1
fi

echo "========================================"
echo "Ligero HTTP Prover Service"
echo "========================================"
echo "Bind address:    $BIND_ADDR"
echo "Proof outputs:   $PROOF_OUTPUTS"
echo "Log level:       ${RUST_LOG}"
echo "Runner dir:      $RUNNER_DIR"
if [[ -n "$THREADS" ]]; then
    echo "Worker threads:  $THREADS"
fi
echo ""

# Build the binary from the git checkout
echo "Building ligero-http-server..."
cargo build --release --manifest-path "$RUNNER_DIR/Cargo.toml" --bin ligero-http-server

# The binary is in the ligero-webgpu-runner's target directory
BINARY="$RUNNER_DIR/target/release/ligero-http-server"

if [[ ! -x "$BINARY" ]]; then
    echo "❌ Binary not found at: $BINARY"
    exit 1
fi

# Construct arguments
ARGS=("-b" "$BIND_ADDR" "--proof-outputs" "$PROOF_OUTPUTS")
if [[ -n "$THREADS" ]]; then
    ARGS+=("-t" "$THREADS")
fi
if [[ -n "$KEEP_PROOF_DIRS" ]]; then
    ARGS+=("--keep-proof-dir")
fi

echo ""
echo "🚀 Starting Ligero HTTP prover service..."
echo "   Endpoints:"
echo "     POST http://${BIND_ADDR}/prove  - Generate proof"
echo "     POST http://${BIND_ADDR}/verify - Verify proof"
echo "     GET  http://${BIND_ADDR}/health - Health check"
echo ""

exec "$BINARY" "${ARGS[@]}"
