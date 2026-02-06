#!/bin/bash
set -e

# Script to run the proof verifier service with proper Ligero configuration
# This sets all required environment variables for Ligero proof verification

# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk

# -----------------------------------------------------------------------------
# Ligero guest program selection
#
# IMPORTANT: Sovereign should pass a **circuit name** to `ligero-runner`, not a filesystem path.
# `ligero-runner` will resolve the correct `.wasm` internally.
#
# Common values:
# - note_spend_guest
# - value_validator_rust
#
# You can still override program selection with:
# - LIGERO_PROGRAM_PATH (either a circuit name OR a full path to a `.wasm`)
# -----------------------------------------------------------------------------

# Default to the Midnight circuit name (not a path).
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-note_spend_guest}"
export LIGERO_PACKING=8192  # Must match the packing used during proof generation

#
# NOTE: We intentionally do NOT check `-f $LIGERO_PROGRAM_PATH` here, because it may be a
# circuit name (e.g. "note_spend_guest"), not a file path.

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
# Optional override. If unset, proof-verifier defaults to number of CPUs.
MAX_CONCURRENT="${MAX_CONCURRENT:-${MAX_CONCURRENT_VERIFICATIONS:-}}"
ROLLUP_CONFIG_PATH="${ROLLUP_CONFIG_PATH:-$SCRIPT_DIR/rollup_config.toml}"
# Optional remote ligero-http-server URL. If unset, verifier uses local daemon pools.
LIGERO_PROOF_SERVICE_URL="${LIGERO_PROOF_SERVICE_URL:-${PROVER_SERVICE_URL:-}}"

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
if [ -n "$MAX_CONCURRENT" ]; then
    echo "   Max concurrent: $MAX_CONCURRENT (explicit override)"
else
    echo "   Max concurrent: auto (uses CPU core count)"
fi
if [ -n "$LIGERO_PROOF_SERVICE_URL" ]; then
    echo "   Prover mode: remote ($LIGERO_PROOF_SERVICE_URL)"
else
    echo "   Prover mode: local daemon pool"
fi
echo "   Rollup config: $ROLLUP_CONFIG_PATH"
print_pool_fvk_pk_status
echo ""

# Run the verifier service
METHOD_ID_ARGS=()
if [ -n "$METHOD_ID" ]; then
    METHOD_ID_ARGS+=(--method-id "$METHOD_ID")
fi
if [ -n "$MIDNIGHT_METHOD_ID" ]; then
    METHOD_ID_ARGS+=(--midnight-method-id "$MIDNIGHT_METHOD_ID")
fi
MAX_CONCURRENT_ARGS=()
if [ -n "$MAX_CONCURRENT" ]; then
    MAX_CONCURRENT_ARGS+=(--max-concurrent "$MAX_CONCURRENT")
fi
PROVER_SERVICE_ARGS=()
if [ -n "$LIGERO_PROOF_SERVICE_URL" ]; then
    PROVER_SERVICE_ARGS+=(--prover-service-url "$LIGERO_PROOF_SERVICE_URL")
fi

exec "$WORKSPACE_ROOT/target/release/proof-verifier" \
    "${METHOD_ID_ARGS[@]}" \
    "${MAX_CONCURRENT_ARGS[@]}" \
    "${PROVER_SERVICE_ARGS[@]}" \
    --bind "$BIND_ADDR" \
    --node-rpc-url "$NODE_RPC_URL" \
    --signing-key-path "$SIGNING_KEY_PATH" \
    --chain-id "$CHAIN_ID" \
    --log-level "$LOG_LEVEL" \
    --rollup-config-path "$ROLLUP_CONFIG_PATH" \
    $DEFER_FLAG
