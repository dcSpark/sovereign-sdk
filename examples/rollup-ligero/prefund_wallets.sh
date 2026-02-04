#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

OUT_FILE="${1:-${PREFUND_OUTPUT_FILE:-$SCRIPT_DIR/prefunded_wallets.jsonl}}"
COUNT="${2:-${PREFUND_COUNT:-1000}}"
CONCURRENCY="${3:-${PREFUND_CONCURRENCY:-10}}"

export RUST_LOG="${RUST_LOG:-info}"
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://localhost:12346}"
export VERIFIER_URL="${VERIFIER_URL:-http://localhost:8080}"
export INDEXER_URL="${INDEXER_URL:-http://localhost:13100}"

# Funding config (defaults match mcp-external's AUTO_FUND_* envs for convenience)
export PREFUND_DEPOSIT_AMOUNT="${PREFUND_DEPOSIT_AMOUNT:-${AUTO_FUND_DEPOSIT_AMOUNT:-100}}"
export PREFUND_GAS_RESERVE="${PREFUND_GAS_RESERVE:-${AUTO_FUND_GAS_RESERVE:-1000000}}"

# Required: wallet that will fund new wallets (must have sufficient balance)
export ADMIN_WALLET_PRIVATE_KEY="${ADMIN_WALLET_PRIVATE_KEY:-}"

export PREFUND_OUTPUT_FILE="$OUT_FILE"
export PREFUND_COUNT="$COUNT"
export PREFUND_CONCURRENCY="$CONCURRENCY"
export PREFUND_APPEND="${PREFUND_APPEND:-true}"

if [[ -z "${ADMIN_WALLET_PRIVATE_KEY}" ]]; then
  echo "ADMIN_WALLET_PRIVATE_KEY is required" >&2
  echo "Usage: $0 [out_file] [count] [concurrency]" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT_FILE")"

# Create secrets file with restrictive permissions
umask 077

cd "$WORKSPACE_ROOT"
cargo run -p mcp-external --bin prefund_wallets --release

echo
echo "Done."
echo "Set PREFUNDED_WALLETS_FILE=$OUT_FILE when starting mcp-external."
