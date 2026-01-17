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
# Find ligero-prover git checkout (dynamically from Cargo.toml)
# -----------------------------------------------------------------------------

CARGO_TOML="$SCRIPT_DIR/Cargo.toml"

# Extract the ligero-runner rev from Cargo.toml
LIGERO_REV=$(grep 'ligero-runner' "$CARGO_TOML" | grep -oE '[a-f0-9]{40}' | head -1)

if [[ -z "$LIGERO_REV" ]]; then
    echo "❌ Could not find ligero-runner rev in $CARGO_TOML"
    exit 1
fi

# Cargo uses the first 7 characters of the commit hash for subdirectories
LIGERO_REV_SHORT="${LIGERO_REV:0:7}"

# Resolve cargo home (respects CARGO_HOME for CI/Nix/isolated builds)
CARGO_GIT_CHECKOUTS="${CARGO_HOME:-$HOME/.cargo}/git/checkouts"

# Find the checkout: $CARGO_HOME/git/checkouts/ligero-prover-<url-hash>/<commit-prefix>/
# First find the ligero-prover repo checkout (hash based on git URL)
LIGERO_REPO=$(find "$CARGO_GIT_CHECKOUTS" -maxdepth 1 -type d -name "ligero-prover-*" 2>/dev/null | head -1)

if [[ -z "$LIGERO_REPO" ]]; then
    echo "❌ Could not find ligero-prover repo in $CARGO_GIT_CHECKOUTS/"
    echo "   Make sure you've run 'cargo build' at least once to fetch dependencies."
    exit 1
fi

# Then find the commit subdirectory (try exact match first, then any available)
LIGERO_CHECKOUT=$(find "$LIGERO_REPO" -maxdepth 1 -type d -name "${LIGERO_REV_SHORT}*" 2>/dev/null | head -1)

if [[ -z "$LIGERO_CHECKOUT" ]]; then
    # Fallback: use any available commit checkout (there's usually only one)
    LIGERO_CHECKOUT=$(find "$LIGERO_REPO" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | head -1)
    if [[ -n "$LIGERO_CHECKOUT" ]]; then
        FOUND_REV=$(basename "$LIGERO_CHECKOUT")
        echo "⚠️  Warning: Expected commit $LIGERO_REV_SHORT but found $FOUND_REV"
        echo "   Run 'cargo build' to fetch the correct version, or using available checkout."
    fi
fi

if [[ -z "$LIGERO_CHECKOUT" ]]; then
    echo "❌ Could not find any commit checkout in $LIGERO_REPO"
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

# Determine target directory (honor CARGO_TARGET_DIR if set)
TARGET_DIR="${CARGO_TARGET_DIR:-$RUNNER_DIR/target}"

# Build the binary from the git checkout
echo "Building ligero-http-server..."
cargo build --release --manifest-path "$RUNNER_DIR/Cargo.toml" --target-dir "$TARGET_DIR" --bin ligero-http-server

# The binary is in the target directory
BINARY="$TARGET_DIR/release/ligero-http-server"

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
