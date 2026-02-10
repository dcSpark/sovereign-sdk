#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

redact_db_url() {
  local url="$1"
  case "$url" in
    *"://"*)
      local prefix="${url%%://*}://"
      local rest="${url#*://}"
      local at="${rest%%@*}"
      local after_at="${rest#*@}"
      if [[ "$rest" == "$after_at" ]]; then
        echo "$url"
        return 0
      fi
      local end_userinfo="${rest%%[/?]*}"
      if [[ "${#at}" -gt "${#end_userinfo}" ]]; then
        echo "$url"
        return 0
      fi
      case "$at" in
        *:*)
          local user="${at%%:*}"
          echo "${prefix}${user}:***@${after_at}"
          return 0
          ;;
      esac
      ;;
  esac
  echo "$url"
}

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
  echo "Generated AUTH_TOKEN for midnight-proof-pool-service."
fi

export MAX_PROOFS="${MAX_PROOFS:-10}"
export PROOF_POOL_BIND_ADDR="${PROOF_POOL_BIND_ADDR:-127.0.0.1:11235}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://127.0.0.1:12346}"
export INDEXER_URL="${INDEXER_URL:-$(default_url_from_bind "${INDEXER_BIND:-127.0.0.1:13100}" 13100)}"
export LIGERO_PROOF_SERVICE_URL="${LIGERO_PROOF_SERVICE_URL:-$(default_url_from_bind "${PROVER_BIND_ADDR:-0.0.0.0:8080}" 8080)}"
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-note_spend_guest}"
export WALLET_SETUP_BATCH_SIZE="${WALLET_SETUP_BATCH_SIZE:-5}"
export WALLET_SETUP_BACKOFF_MS="${WALLET_SETUP_BACKOFF_MS:-1000}"
export SEQUENCER_READY_CHECK_TIMEOUT_MS="${SEQUENCER_READY_CHECK_TIMEOUT_MS:-2000}"
export ADMIN_WALLET_PRIVATE_KEY="${ADMIN_WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-sqlite://$SCRIPT_DIR/demo_data/da.sqlite?mode=rwc}"

echo "========================================"
echo "Midnight Proof Pool Service"
echo "========================================"
echo "Bind address:           $PROOF_POOL_BIND_ADDR"
echo "MAX_PROOFS:             $MAX_PROOFS"
echo "WALLET_SETUP_BATCH_SIZE: $WALLET_SETUP_BATCH_SIZE"
echo "WALLET_SETUP_BACKOFF_MS: $WALLET_SETUP_BACKOFF_MS"
echo "SEQUENCER_READY_CHECK_TIMEOUT_MS: $SEQUENCER_READY_CHECK_TIMEOUT_MS"
echo "ROLLUP_RPC_URL:         $ROLLUP_RPC_URL"
echo "INDEXER_URL:            $INDEXER_URL"
echo "LIGERO_PROOF_SERVICE_URL: $LIGERO_PROOF_SERVICE_URL"
echo "DA_CONNECTION_STRING:   $(redact_db_url "$DA_CONNECTION_STRING")"
echo ""
echo "Endpoints:"
echo "  GET  http://${PROOF_POOL_BIND_ADDR}/status?auth_token=$AUTH_TOKEN"
echo "  GET  http://${PROOF_POOL_BIND_ADDR}/max_proofs?auth_token=$AUTH_TOKEN&max_proofs=25"
echo "  GET  http://${PROOF_POOL_BIND_ADDR}/send?auth_token=$AUTH_TOKEN&proof_quantity=25"
echo "  GET  http://${PROOF_POOL_BIND_ADDR}/burst?auth_token=$AUTH_TOKEN&proof_quantities=2,5,10"
echo ""

cd "$WORKSPACE_ROOT/examples/rollup-ligero"
exec cargo run -p midnight-proof-pool-service --release
