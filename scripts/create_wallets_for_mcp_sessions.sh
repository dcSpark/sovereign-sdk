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
  --jobs <N>            Number of parallel workers (default: 8, env: MCP_WALLET_BATCH_JOBS)
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
JOBS="${MCP_WALLET_BATCH_JOBS:-8}"

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
    --jobs)
      JOBS="${2:-}"
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

case "$JOBS" in
  *[!0-9]*|'')
    echo "--jobs must be a positive integer" >&2
    exit 2
    ;;
esac
if [ "$JOBS" -lt 1 ]; then
  echo "--jobs must be >= 1" >&2
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

RESULTS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/mcp-wallet-batch.XXXXXX")"
cleanup() {
  rm -rf "$RESULTS_DIR"
}
trap cleanup EXIT

new_jsonrpc_id() {
  local now_ns
  now_ns="$(date +%s%N 2>/dev/null || date +%s)"
  printf '%s-%s-%s' "$$" "$now_ns" "$RANDOM"
}

extract_json_payload() {
  local resp="$1"
  local first_data_line

  if printf '%s' "$resp" | jq -e . >/dev/null 2>&1; then
    printf '%s' "$resp"
    return 0
  fi

  first_data_line="$(printf '%s\n' "$resp" | sed -n 's/^data: //p' | head -n 1)"
  if [ -n "$first_data_line" ] && printf '%s' "$first_data_line" | jq -e . >/dev/null 2>&1; then
    printf '%s' "$first_data_line"
    return 0
  fi

  return 1
}

extract_error_message() {
  local resp="$1"
  local json_payload
  local err_msg
  local is_error
  local text_msg

  json_payload="$(extract_json_payload "$resp" 2>/dev/null || true)"
  if [ -z "$json_payload" ]; then
    echo "Unparseable MCP response"
    return 0
  fi

  err_msg="$(printf '%s' "$json_payload" | jq -r '.error.message // empty' 2>/dev/null || true)"
  if [ -n "$err_msg" ]; then
    echo "$err_msg"
    return 0
  fi

  is_error="$(printf '%s' "$json_payload" | jq -r '.result.isError // false' 2>/dev/null || true)"
  if [ "$is_error" = "true" ]; then
    text_msg="$(printf '%s' "$json_payload" | jq -r '.result.content[]? | select(.type=="text") | .text' 2>/dev/null | head -n 1 || true)"
    if [ -n "$text_msg" ]; then
      echo "$text_msg"
    else
      echo "MCP tool call returned isError=true"
    fi
  fi
}

extract_wallet_field() {
  local resp="$1"
  local field="$2"
  local json_payload
  local tool_text

  json_payload="$(extract_json_payload "$resp" 2>/dev/null || true)"
  if [ -z "$json_payload" ]; then
    return 0
  fi

  tool_text="$(printf '%s' "$json_payload" | jq -r '.result.content[]? | select(.type=="text") | .text' 2>/dev/null | head -n 1 || true)"
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
  local request_id
  local payload response err

  request_id="$(new_jsonrpc_id)"
  payload="$(jq -nc \
    --arg id "$request_id" \
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
  local request_id
  local payload

  request_id="$(new_jsonrpc_id)"
  payload="$(jq -nc \
    --arg id "$request_id" \
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
  rpc_post "$session_id" "$payload"
}

sanitize_error() {
  local msg="$1"
  msg="${msg//$'\n'/ }"
  msg="${msg//$'\r'/ }"
  msg="${msg//$'\t'/ }"
  printf '%s' "$msg"
}

process_session() {
  local session_id="$1"
  local index="$2"
  local total="$3"
  local result_file="$4"

  local init_err
  local remove_resp
  local remove_err
  local create_resp
  local create_err
  local wallet_address
  local privacy_address

  echo "[$index/$total] session_id=$session_id"

  if ! init_err="$(rpc_initialize "$session_id" 2>/dev/null)"; then
    init_err="$(sanitize_error "$init_err")"
    echo "  FAIL initialize: $init_err"
    printf 'FAIL\t%s\tinitialize\t%s\n' "$session_id" "$init_err" >"$result_file"
    return 1
  fi

  if [ "$REMOVE_FIRST" -eq 1 ]; then
    remove_resp="$(rpc_call_tool "$session_id" "removeWallet")"
    remove_err="$(extract_error_message "$remove_resp")"
    if [ -n "$remove_err" ]; then
      remove_err="$(sanitize_error "$remove_err")"
      echo "  FAIL removeWallet: $remove_err"
      printf 'FAIL\t%s\tremoveWallet\t%s\n' "$session_id" "$remove_err" >"$result_file"
      return 1
    fi
  fi

  create_resp="$(rpc_call_tool "$session_id" "createWallet")"
  create_err="$(extract_error_message "$create_resp")"
  if [ -n "$create_err" ]; then
    create_err="$(sanitize_error "$create_err")"
    echo "  FAIL createWallet: $create_err"
    printf 'FAIL\t%s\tcreateWallet\t%s\n' "$session_id" "$create_err" >"$result_file"
    return 1
  fi

  wallet_address="$(extract_wallet_field "$create_resp" "wallet_address")"
  privacy_address="$(extract_wallet_field "$create_resp" "privacy_address")"
  if [ -n "$wallet_address" ] || [ -n "$privacy_address" ]; then
    echo "  OK wallet_address=${wallet_address:-n/a} privacy_address=${privacy_address:-n/a}"
  else
    echo "  OK wallet created"
  fi
  printf 'OK\t%s\t%s\t%s\n' "$session_id" "${wallet_address:-}" "${privacy_address:-}" >"$result_file"
}

wait_for_slot() {
  while [ "$(jobs -rp | wc -l | tr -d '[:space:]')" -ge "$JOBS" ]; do
    sleep 0.05
  done
}

echo "Found ${#SESSION_IDS[@]} session(s) in mcp_sessions."
echo "MCP endpoint: $MCP_ENDPOINT"
echo "Parallel workers: $JOBS"
if [ "$REMOVE_FIRST" -eq 1 ]; then
  echo "Mode: rotate wallet (removeWallet -> createWallet)"
else
  echo "Mode: create only (keep existing wallet if already loaded)"
fi
echo

total_sessions="${#SESSION_IDS[@]}"
pids=()
index=0
for session_id in "${SESSION_IDS[@]}"; do
  index=$((index + 1))
  result_file="$(printf '%s/%06d.result' "$RESULTS_DIR" "$index")"
  wait_for_slot
  process_session "$session_id" "$index" "$total_sessions" "$result_file" &
  pids+=("$!")
done

had_worker_errors=0
for pid in "${pids[@]}"; do
  if ! wait "$pid"; then
    had_worker_errors=1
  fi
done

success_count=0
fail_count=0
for result_file in "$RESULTS_DIR"/*.result; do
  [ -e "$result_file" ] || continue
  IFS=$'\t' read -r status session_id step_or_wallet extra <"$result_file" || true
  if [ "$status" = "OK" ]; then
    success_count=$((success_count + 1))
  elif [ "$status" = "FAIL" ]; then
    fail_count=$((fail_count + 1))
  fi
done

echo
echo "Done. Success: $success_count  Failed: $fail_count"
if [ "$fail_count" -gt 0 ]; then
  echo
  echo "Failed sessions:"
  for result_file in "$RESULTS_DIR"/*.result; do
    [ -e "$result_file" ] || continue
    IFS=$'\t' read -r status session_id step message <"$result_file" || true
    if [ "$status" = "FAIL" ]; then
      echo "  - $session_id [$step] $message"
    fi
  done
fi

if [ "$fail_count" -gt 0 ] || [ "$had_worker_errors" -ne 0 ]; then
  exit 1
fi
