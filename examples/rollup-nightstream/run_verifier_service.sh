#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${SERVICE_TARGET_DIR:-release}"
BIN="$WORKSPACE_ROOT/target/$TARGET_DIR/proof-verifier"

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk

METHOD_ID="${METHOD_ID:-}"
MIDNIGHT_METHOD_ID="${MIDNIGHT_METHOD_ID:-}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:8080}"
NODE_RPC_URL="${NODE_RPC_URL:-${ROLLUP_RPC_URL:-http://127.0.0.1:12346}}"
SIGNING_KEY_PATH="${SIGNING_KEY_PATH:-$WORKSPACE_ROOT/examples/test-data/keys/token_deployer_private_key.json}"
CHAIN_ID="${CHAIN_ID:-4321}"
LOG_LEVEL="${LOG_LEVEL:-info}"
MAX_CONCURRENT="${MAX_CONCURRENT:-${MAX_CONCURRENT_VERIFICATIONS:-}}"
ROLLUP_CONFIG_PATH="${ROLLUP_CONFIG_PATH:-$SCRIPT_DIR/rollup_config.toml}"
PROVER_SERVICE_URL="${PROVER_SERVICE_URL:-${NIGHTSTREAM_PROOF_SERVICE_URL:-${LIGERO_PROOF_SERVICE_URL:-}}}"

DEFER_FLAG=""
DSS="${DEFER_SEQUENCER_SUBMISSION:-${DEFER_SUBMISSION:-}}"
if [ -n "$DSS" ]; then
  DSS_LC=$(printf "%s" "$DSS" | tr '[:upper:]' '[:lower:]')
  case "$DSS_LC" in
    1|true|yes|on) DEFER_FLAG="--defer-submission" ;;
  esac
fi

echo "Nightstream Proof Verifier Service"
echo "  Bind address: $BIND_ADDR"
echo "  Node RPC:     $NODE_RPC_URL"
echo "  Log level:    $LOG_LEVEL"
echo "  Rollup config: $ROLLUP_CONFIG_PATH"
if [ -n "$MAX_CONCURRENT" ]; then
  echo "  Max concurrent: $MAX_CONCURRENT"
fi
if [ -n "$PROVER_SERVICE_URL" ]; then
  echo "  Prover mode:  remote ($PROVER_SERVICE_URL)"
fi
print_pool_fvk_pk_status

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
if [ -n "$PROVER_SERVICE_URL" ]; then
  PROVER_SERVICE_ARGS+=(--prover-service-url "$PROVER_SERVICE_URL")
fi

if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build ${TARGET_DIR/release/--release }-p sov-proof-verifier-service"
  exit 1
fi

exec "$BIN" \
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
