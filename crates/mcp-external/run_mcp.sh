#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

export RUST_LOG="${RUST_LOG:-debug}"
export MCP_SERVER_BIND_ADDRESS="${MCP_SERVER_BIND_ADDRESS:-0.0.0.0:3000}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://localhost:12346}"
export VERIFIER_URL="${VERIFIER_URL:-http://localhost:8080}"
export INDEXER_URL="${INDEXER_URL:-http://localhost:13100}"
export AUTO_FUND_DEPOSIT_AMOUNT="${AUTO_FUND_DEPOSIT_AMOUNT:-50}"

export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-$WORKSPACE_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm}"
export LIGERO_PROVER_BINARY_PATH="${LIGERO_PROVER_BINARY_PATH:-$WORKSPACE_ROOT/crates/adapters/ligero/bins/macos-arm64/bin/webgpu_prover}"
export LIGERO_SHADER_PATH="${LIGERO_SHADER_PATH:-$WORKSPACE_ROOT/crates/adapters/ligero/bins/shader}"

if [[ ! -f "$LIGERO_PROGRAM_PATH" ]]; then
  echo "LIGERO_PROGRAM_PATH not found: $LIGERO_PROGRAM_PATH"
  exit 1
fi

if [[ ! -f "$LIGERO_PROVER_BINARY_PATH" ]]; then
  echo "LIGERO_PROVER_BINARY_PATH not found: $LIGERO_PROVER_BINARY_PATH"
  exit 1
fi

if [[ ! -d "$LIGERO_SHADER_PATH" ]]; then
  echo "LIGERO_SHADER_PATH not found: $LIGERO_SHADER_PATH"
  exit 1
fi

cd "$WORKSPACE_ROOT"
exec cargo run -p mcp-external
