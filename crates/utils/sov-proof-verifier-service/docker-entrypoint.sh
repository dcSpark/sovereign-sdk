#!/bin/sh
set -e

# Support both env var naming conventions (Docker style and shell script style)
BIND="${BIND_ADDR:-${BIND_ADDRESS:-0.0.0.0:8080}}"
MAX_CONC="${MAX_CONCURRENT:-${MAX_CONCURRENT_VERIFICATIONS:-10}}"
ROLLUP_CFG="${ROLLUP_CONFIG_PATH:-/app/rollup_config.toml}"

set -- \
  --bind "${BIND}" \
  --node-rpc-url "${NODE_RPC_URL:-http://127.0.0.1:12346}" \
  --signing-key-path "${SIGNING_KEY_PATH:-/app/keys/token_deployer_private_key.json}" \
  --chain-id "${CHAIN_ID:-4321}" \
  --max-concurrent "${MAX_CONC}" \
  --log-level "${LOG_LEVEL:-info}" \
  --rollup-config-path "${ROLLUP_CFG}" \
  "$@"

if [ -n "${METHOD_ID:-}" ]; then
  set -- --method-id "${METHOD_ID}" "$@"
fi

if [ -n "${MIDNIGHT_METHOD_ID:-}" ]; then
  set -- --midnight-method-id "${MIDNIGHT_METHOD_ID}" "$@"
fi

if [ -n "${DA_DB:-}" ]; then
  set -- --da-db "${DA_DB}" "$@"
fi

# Support both DEFER_SUBMISSION and DEFER_SEQUENCER_SUBMISSION
DEFER="${DEFER_SUBMISSION:-${DEFER_SEQUENCER_SUBMISSION:-false}}"
case "$(printf '%s' "$DEFER" | tr '[:upper:]' '[:lower:]')" in
  1|true|yes|on)
    set -- --defer-submission "$@"
    ;;
esac

exec /usr/local/bin/proof-verifier "$@"
