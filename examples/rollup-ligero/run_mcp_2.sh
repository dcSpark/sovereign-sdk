#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk
print_pool_fvk_pk_status

export RUST_LOG="${RUST_LOG:-debug}"
export MCP_SERVER_BIND_ADDRESS="${MCP_SERVER_BIND_ADDRESS_2:-0.0.0.0:3001}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://localhost:12346}"
export VERIFIER_URL="${VERIFIER_URL:-http://localhost:8080}"
export INDEXER_URL="${INDEXER_URL:-http://localhost:13100}"
export MIDNIGHT_FVK_SERVICE_URL="${MIDNIGHT_FVK_SERVICE_URL:-http://127.0.0.1:8088}"
export AUTO_FUND_DEPOSIT_AMOUNT="${AUTO_FUND_DEPOSIT_AMOUNT_2:-50}"
export WALLET_PRIVATE_KEY="${WALLET_PRIVATE_KEY_2:-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2}"
export PRIVPOOL_SPEND_KEY="${PRIVPOOL_SPEND_KEY_2:-0xc34f1eab2d9f96ae8dca98fb5bbde69dce135b936ce514f060b0e6c6bbb9c660}"

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
exec cargo run -p mcp-external --release
