#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

normalize_host() {
  local host="$1"
  if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
    echo "127.0.0.1"
    return
  fi
  echo "$host"
}

default_url_from_bind() {
  local bind="$1"
  local default_port="$2"
  local host="${bind%:*}"
  local port="${bind##*:}"
  if [[ "$host" == "$port" ]]; then
    host="$bind"
    port="$default_port"
  fi
  host="$(normalize_host "$host")"
  echo "http://${host}:${port}"
}

if [[ -z "${AUTH_TOKEN:-}" ]]; then
  if command -v openssl >/dev/null 2>&1; then
    export AUTH_TOKEN="$(openssl rand -hex 16)"
  else
    export AUTH_TOKEN="$(LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 32)"
  fi
fi

export MAX_PROOFS="${MAX_PROOFS:-1}"
export PROOF_POOL_BIND_ADDR="${PROOF_POOL_BIND_ADDR:-127.0.0.1:11235}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://127.0.0.1:12346}"
export INDEXER_URL="${INDEXER_URL:-$(default_url_from_bind "${INDEXER_BIND:-127.0.0.1:13100}" 13100)}"
export LIGERO_PROOF_SERVICE_URL="${LIGERO_PROOF_SERVICE_URL:-$(default_url_from_bind "${PROVER_BIND_ADDR:-0.0.0.0:8080}" 8080)}"
export WALLET_SETUP_BACKOFF_MS="${WALLET_SETUP_BACKOFF_MS:-1000}"
export SEQUENCER_READY_CHECK_TIMEOUT_MS="${SEQUENCER_READY_CHECK_TIMEOUT_MS:-2000}"
export MAX_CONCURRENT_PROOFS="${MAX_CONCURRENT_PROOFS:-5}"
export ADMIN_WALLET_PRIVATE_KEY="${ADMIN_WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-postgresql://admin:1234@localhost:5432/da}"
mkdir -p "$SCRIPT_DIR/demo_data"
export POOL_STATE_FILE="${POOL_STATE_FILE:-$SCRIPT_DIR/demo_data/pool_state.sqlite}"

echo "Midnight Proof Pool Service"
echo "  Bind address:   $PROOF_POOL_BIND_ADDR"
echo "  MAX_PROOFS:     $MAX_PROOFS"
echo "  ROLLUP_RPC_URL: $ROLLUP_RPC_URL"
echo "  INDEXER_URL:    $INDEXER_URL"

cd "$WORKSPACE_ROOT/examples/rollup-nightstream"
exec cargo run -p midnight-proof-pool-service --release
