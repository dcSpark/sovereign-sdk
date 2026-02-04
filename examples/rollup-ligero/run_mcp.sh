#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk
print_pool_fvk_pk_status

export RUST_LOG="${RUST_LOG:-info}"
export MCP_SERVER_BIND_ADDRESS="${MCP_SERVER_BIND_ADDRESS:-0.0.0.0:3000}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://localhost:12346}"
export VERIFIER_URL="${VERIFIER_URL:-http://localhost:8080}"
export INDEXER_URL="${INDEXER_URL:-http://localhost:13100}"
export MIDNIGHT_FVK_SERVICE_URL="${MIDNIGHT_FVK_SERVICE_URL:-http://127.0.0.1:8088}"
export AUTO_FUND_DEPOSIT_AMOUNT="${AUTO_FUND_DEPOSIT_AMOUNT:-100}"
export AUTO_FUND_GAS_RESERVE="${AUTO_FUND_GAS_RESERVE:-1000000}"
export ADMIN_WALLET_PRIVATE_KEY="${ADMIN_WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"
export WALLET_PRIVATE_KEY="${WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"
export PRIVPOOL_SPEND_KEY="${PRIVPOOL_SPEND_KEY:-0xb23e0dc9d1f8869c8bc87ab4eaacd58cbe024a825bdc13fefa4d4c5eaa0b855f}"

# Persistent session storage configuration
if [[ -z "${MCP_SESSION_DB_URL:-}" ]]; then
  echo "MCP_SESSION_DB_URL is required but not set"
  exit 1
fi
export MCP_SESSION_DB_URL
export MCP_SESSION_DB_ENCRYPTION_KEY="${MCP_SESSION_DB_ENCRYPTION_KEY:-7e2bdfe834ff9a47c8cdba8cf41c4dcd83410fef61b805ea9740c32335697d12}"
export MCP_AUTO_INITIALIZE_SESSIONS="${MCP_AUTO_INITIALIZE_SESSIONS:-true}"

# Authority API configuration for /authority/* HTTP endpoints
# Uses MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN for protected endpoints (freeze/thaw). If not set, write endpoints are disabled.
# METRICS_API_URL: Base URL for sov-metrics-api, enables /authority/tps endpoint.
export METRICS_API_URL="${METRICS_API_URL:-http://127.0.0.1:13200}"

# Provide the ZK guest program via:
# - LIGERO_PROGRAM_PATH (circuit name like `note_spend_guest` OR full path to a `.wasm` file)
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-note_spend_guest}"
# Sovereign no longer vendors Ligero binaries/shaders.
# Prefer `ligero-runner` auto-discovery. If you want to override discovery,
# set these env vars explicitly before running this script:
# - LIGERO_PROVER_BIN or LIGERO_PROVER_BINARY_PATH
# - LIGERO_VERIFIER_BIN
# - LIGERO_SHADER_PATH

# If the user passed a path, fail fast if it doesn't exist.
if [[ "$LIGERO_PROGRAM_PATH" == *"/"* || "$LIGERO_PROGRAM_PATH" == *".wasm" ]]; then
  if [[ ! -f "$LIGERO_PROGRAM_PATH" ]]; then
    echo "LIGERO_PROGRAM_PATH not found: $LIGERO_PROGRAM_PATH"
    exit 1
  fi
fi

if [[ -n "${LIGERO_PROVER_BINARY_PATH:-}" && ! -f "$LIGERO_PROVER_BINARY_PATH" ]]; then
  echo "LIGERO_PROVER_BINARY_PATH not found: $LIGERO_PROVER_BINARY_PATH"
  exit 1
fi

if [[ -n "${LIGERO_SHADER_PATH:-}" && ! -d "$LIGERO_SHADER_PATH" ]]; then
  echo "LIGERO_SHADER_PATH not found: $LIGERO_SHADER_PATH"
  exit 1
fi

cd "$WORKSPACE_ROOT"
exec cargo run -p mcp-external --bin mcp-external --release
