#!/bin/sh
set -e

set -- \
  --bind "${BIND_ADDRESS:-0.0.0.0:8080}" \
  --node-rpc-url "${NODE_RPC_URL:-http://127.0.0.1:12346}" \
  --signing-key-path "${SIGNING_KEY_PATH:-/app/keys/token_deployer_private_key.json}" \
  --chain-id "${CHAIN_ID:-4321}" \
  --max-concurrent "${MAX_CONCURRENT_VERIFICATIONS:-10}" \
  --log-level "${LOG_LEVEL:-info}" \
  "$@"

if [ -n "${METHOD_ID:-}" ]; then
  set -- --method-id "${METHOD_ID}" "$@"
fi

exec /usr/local/bin/proof-verifier "$@"
