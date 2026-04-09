#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${SERVICE_TARGET_DIR:-release}"
BIN="$WORKSPACE_ROOT/target/$TARGET_DIR/mcp-external"

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk

export RUST_LOG="${RUST_LOG:-info}"
export MCP_SERVER_BIND_ADDRESS="${MCP_SERVER_BIND_ADDRESS:-0.0.0.0:3000}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://localhost:12346}"
export VERIFIER_URL="${VERIFIER_URL:-http://localhost:8080}"
export INDEXER_URL="${INDEXER_URL:-http://localhost:13100}"
export MIDNIGHT_FVK_SERVICE_URL="$(midnight_fvk_service_url)"
export AUTO_FUND_DEPOSIT_AMOUNT="${AUTO_FUND_DEPOSIT_AMOUNT:-1000}"
export AUTO_FUND_GAS_RESERVE="${AUTO_FUND_GAS_RESERVE:-1000000}"
export ADMIN_WALLET_PRIVATE_KEY="${ADMIN_WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"

if [[ -n "${MCP_SESSION_DB_URL:-}" ]]; then
  export MCP_SESSION_DB_URL
else
  echo "MCP_SESSION_DB_URL not set; MCP session persistence disabled."
fi
export MCP_SESSION_DB_ENCRYPTION_KEY="${MCP_SESSION_DB_ENCRYPTION_KEY:-7e2bdfe834ff9a47c8cdba8cf41c4dcd83410fef61b805ea9740c32335697d12}"
export MCP_AUTO_INITIALIZE_SESSIONS="${MCP_AUTO_INITIALIZE_SESSIONS:-true}"
export MCP_AUTO_CREATE_WALLET="${MCP_AUTO_CREATE_WALLET:-false}"

export METRICS_API_URL="${METRICS_API_URL:-http://127.0.0.1:13200}"

echo "MCP External Service"
echo "  Bind:        $MCP_SERVER_BIND_ADDRESS"
echo "  Rollup RPC:  $ROLLUP_RPC_URL"
echo "  Verifier:    $VERIFIER_URL"
echo "  Indexer:     $INDEXER_URL"
echo "  FVK URL:     $MIDNIGHT_FVK_SERVICE_URL"
print_pool_fvk_pk_status

auto_create_wallet_enabled=0
case "$(printf "%s" "${MCP_AUTO_CREATE_WALLET:-false}" | tr '[:upper:]' '[:lower:]')" in
  1|true|yes|on) auto_create_wallet_enabled=1 ;;
esac

if [[ -n "${POOL_FVK_PK:-}" && "$auto_create_wallet_enabled" -eq 1 ]]; then
  wait_for_midnight_fvk_service
fi

if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build ${TARGET_DIR/release/--release }-p mcp-external"
  exit 1
fi

cd "$WORKSPACE_ROOT"
exec "$BIN"
