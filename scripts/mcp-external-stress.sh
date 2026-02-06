#!/bin/sh
set -eu

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/mcp-external-stress.sh --wallets <N> --txs <N> [options]

Required:
  --wallets <N>        Number of concurrent MCP sessions (one wallet per session)
  --txs <N>            Transactions per wallet session

Options:
  --endpoint <URL>     MCP endpoint (default: https://midnight-l2-testnet.shinkai.com/mcp/mcp)
  --session-ids-file <PATH>
                      File with stable MCP session IDs to reuse across runs
                      (default: <repo>/.mcp-external-stress-session-ids.txt)
  --send-amount <N>    Amount (dust) per tx (default: 1000)
  --confirm            Poll confirmation (adds extra load)
  --rust-log <SPEC>    Override RUST_LOG (default: mcp_external_stress=info,rmcp=warn)
  -h, --help           Show help

Environment variables (optional, flags override):
  MCP_ENDPOINT
  MCP_STRESS_SESSION_IDS_FILE
  SEND_AMOUNT
  CONFIRM
  MCP_STRESS_RUST_LOG
EOF
}

WALLETS=""
TXS_PER_WALLET=""
SESSION_IDS_FILE=""

MCP_ENDPOINT="${MCP_ENDPOINT:-https://midnight-l2-testnet.shinkai.com/mcp/mcp}"
SEND_AMOUNT="${SEND_AMOUNT:-1000}"
CONFIRM="${CONFIRM:-0}"
RUST_LOG="${MCP_STRESS_RUST_LOG:-mcp_external_stress=info,rmcp=warn}"

# Backwards-compatible positional args: <wallets> <txs-per-wallet>
if [ "$#" -eq 2 ] && [ "${1#-}" = "$1" ] && [ "${2#-}" = "$2" ]; then
  WALLETS="$1"
  TXS_PER_WALLET="$2"
  shift 2
else
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --wallets|-w)
        WALLETS="${2:-}"
        shift 2
        ;;
      --txs|--txs-per-wallet|-t)
        TXS_PER_WALLET="${2:-}"
        shift 2
        ;;
      --endpoint|--mcp-endpoint)
        MCP_ENDPOINT="${2:-}"
        shift 2
        ;;
      --session-ids-file)
        SESSION_IDS_FILE="${2:-}"
        shift 2
        ;;
      --send-amount)
        SEND_AMOUNT="${2:-}"
        shift 2
        ;;
      --confirm)
        CONFIRM=1
        shift
        ;;
      --rust-log)
        RUST_LOG="${2:-}"
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      --)
        shift
        break
        ;;
      *)
        echo "Unknown argument: $1" >&2
        usage
        exit 2
        ;;
    esac
  done
fi

if [ -z "$WALLETS" ] || [ -z "$TXS_PER_WALLET" ]; then
  usage
  exit 2
fi

case "$WALLETS" in
  *[!0-9]*|'') echo "--wallets must be a non-negative integer" >&2; exit 2 ;;
esac
case "$TXS_PER_WALLET" in
  *[!0-9]*|'') echo "--txs must be a non-negative integer" >&2; exit 2 ;;
esac

export RUST_LOG

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SESSION_IDS_FILE="${SESSION_IDS_FILE:-${MCP_STRESS_SESSION_IDS_FILE:-$REPO_ROOT/.mcp-external-stress-session-ids.txt}}"

set -- \
  --mcp-endpoint "$MCP_ENDPOINT" \
  --wallets "$WALLETS" \
  --max-txs "$TXS_PER_WALLET" \
  --send-amount "$SEND_AMOUNT" \
  --duration-secs 0 \
  --session-ids-file "$SESSION_IDS_FILE" \
  "$@"

case "$CONFIRM" in
  1|true|TRUE|yes|YES) set -- "$@" --confirm ;;
esac

exec cargo run -p mcp-external-stress --manifest-path "$REPO_ROOT/Cargo.toml" -- "$@"
