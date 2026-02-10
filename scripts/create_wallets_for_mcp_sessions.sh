#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Create fresh wallets for every session_id found in the mcp_sessions table.

Usage:
  scripts/create_wallets_for_mcp_sessions.sh [options]

Options:
  --db-url <URL>        PostgreSQL URL (default: MCP_SESSION_DB_URL env var)
  --mcp-endpoint <URL>  MCP endpoint URL (default: MCP_ENDPOINT env var or http://127.0.0.1:3000/mcp)
  --keep-existing       Do not call removeWallet before createWallet
  -h, --help            Show this help

Requirements:
  - psql
  - curl
  - jq
EOF
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Missing required command: $cmd" >&2
    exit 2
  fi
}

DB_URL="${MCP_SESSION_DB_URL:-}"
MCP_ENDPOINT="${MCP_ENDPOINT:-http://127.0.0.1:3000/mcp}"
REMOVE_FIRST=1

while [ "$#" -gt 0 ]; do
  case "$1" in
    --db-url)
      DB_URL="${2:-}"
      shift 2
      ;;
    --mcp-endpoint)
      MCP_ENDPOINT="${2:-}"
      shift 2
      ;;
    --keep-existing)
      REMOVE_FIRST=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [ -z "$DB_URL" ]; then
  echo "Missing DB URL. Provide --db-url or set MCP_SESSION_DB_URL." >&2
  exit 2
fi

require_cmd psql
require_cmd curl
require_cmd jq

mapfile -t SESSION_IDS < <(
  psql "$DB_URL" -At -c "SELECT session_id FROM mcp_sessions ORDER BY session_id;" \
    | sed '/^[[:space:]]*$/d'
)

if [ "${#SESSION_IDS[@]}" -eq 0 ]; then
  echo "No rows found in mcp_sessions."
  exit 0
fi

jsonrpc_id=1
success_count=0
fail_count=0

extract_error_message() {
  local resp="$1"
  printf '%s' "$resp" | jq -r '.error.message // empty' 2>/dev/null || true
}

extract_wallet_field() {
  local resp="$1"
  local field="$2"
  local tool_text
  tool_text="$(printf '%s' "$resp" | jq -r '.result.content[]? | select(.type=="text") | .text' 2>/dev/null | head -n 1 || true)"
  if [ -z "$tool_text" ]; then
    return 0
  fi
  printf '%s' "$tool_text" | jq -r --arg field "$field" '.[$field] // empty' 2>/dev/null || true
}

rpc_post() {
  local session_id="$1"
  local body="$2"
  curl -sS "$MCP_ENDPOINT" \
    -H 'Accept: application/json, text/event-stream' \
    -H 'Content-Type: application/json' \
    -H "Mcp-Session-Id: $session_id" \
    --data "$body"
}

rpc_initialize() {
  local session_id="$1"
  local payload response err

  payload="$(jq -nc \
    --argjson id "$jsonrpc_id" \
    '{
      "jsonrpc":"2.0",
      "id": $id,
      "method":"initialize",
      "params": {
        "protocolVersion":"2024-11-05",
        "clientInfo":{"name":"wallet-batch-script","version":"1.0.0"},
        "capabilities": {}
      }
    }'
  )"
  jsonrpc_id=$((jsonrpc_id + 1))

  response="$(rpc_post "$session_id" "$payload")"
  err="$(extract_error_message "$response")"
  if [ -n "$err" ]; then
    echo "$err"
    return 1
  fi

  # Best-effort MCP notification.
  rpc_post "$session_id" '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' >/dev/null || true
}

rpc_call_tool() {
  local session_id="$1"
  local tool_name="$2"
  local payload

  payload="$(jq -nc \
    --argjson id "$jsonrpc_id" \
    --arg tool "$tool_name" \
    '{
      "jsonrpc":"2.0",
      "id": $id,
      "method":"tools/call",
      "params": {
        "name": $tool,
        "arguments": {}
      }
    }'
  )"
  jsonrpc_id=$((jsonrpc_id + 1))
  rpc_post "$session_id" "$payload"
}

echo "Found ${#SESSION_IDS[@]} session(s) in mcp_sessions."
echo "MCP endpoint: $MCP_ENDPOINT"
if [ "$REMOVE_FIRST" -eq 1 ]; then
  echo "Mode: rotate wallet (removeWallet -> createWallet)"
else
  echo "Mode: create only (keep existing wallet if already loaded)"
fi
echo

index=0
for session_id in "${SESSION_IDS[@]}"; do
  index=$((index + 1))
  echo "[$index/${#SESSION_IDS[@]}] session_id=$session_id"

  if ! init_err="$(rpc_initialize "$session_id" 2>/dev/null)"; then
    echo "  FAIL initialize: $init_err"
    fail_count=$((fail_count + 1))
    continue
  fi

  if [ "$REMOVE_FIRST" -eq 1 ]; then
    remove_resp="$(rpc_call_tool "$session_id" "removeWallet")"
    remove_err="$(extract_error_message "$remove_resp")"
    if [ -n "$remove_err" ]; then
      echo "  FAIL removeWallet: $remove_err"
      fail_count=$((fail_count + 1))
      continue
    fi
  fi

  create_resp="$(rpc_call_tool "$session_id" "createWallet")"
  create_err="$(extract_error_message "$create_resp")"
  if [ -n "$create_err" ]; then
    echo "  FAIL createWallet: $create_err"
    fail_count=$((fail_count + 1))
    continue
  fi

  wallet_address="$(extract_wallet_field "$create_resp" "wallet_address")"
  privacy_address="$(extract_wallet_field "$create_resp" "privacy_address")"
  if [ -n "$wallet_address" ] || [ -n "$privacy_address" ]; then
    echo "  OK wallet_address=${wallet_address:-n/a} privacy_address=${privacy_address:-n/a}"
  else
    echo "  OK wallet created"
  fi
  success_count=$((success_count + 1))
done

echo
echo "Done. Success: $success_count  Failed: $fail_count"
if [ "$fail_count" -gt 0 ]; then
  exit 1
fi
