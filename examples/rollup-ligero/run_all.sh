#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export WORKSPACE_ROOT

ROLLUP_ARGS=("$@")

WAIT_TIMEOUT_SECONDS="${WAIT_TIMEOUT_SECONDS:-600}"
WAIT_SLEEP_SECONDS="${WAIT_SLEEP_SECONDS:-1}"

PIDS=()
NAMES=()

normalize_host() {
  local host="$1"
  if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
    echo "127.0.0.1"
    return
  fi
  echo "$host"
}

wait_for_port() {
  local name="$1"
  local host="$2"
  local port="$3"
  local pid="$4"
  local waited=0

  host="$(normalize_host "$host")"

  while (( waited < WAIT_TIMEOUT_SECONDS )); do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "Error: $name exited before opening $host:$port"
      return 1
    fi
    if (echo > "/dev/tcp/$host/$port") >/dev/null 2>&1; then
      echo "$name is listening on $host:$port"
      return 0
    fi
    sleep "$WAIT_SLEEP_SECONDS"
    waited=$((waited + WAIT_SLEEP_SECONDS))
  done

  echo "Error: Timed out waiting for $name on $host:$port"
  return 1
}

start_service() {
  local name="$1"
  shift
  "$@" &
  local pid=$!
  PIDS+=("$pid")
  NAMES+=("$name")
  echo "Started $name (pid $pid)"
}

cleanup() {
  local exit_code=$?
  set +e
  if [ "${#PIDS[@]}" -gt 0 ]; then
    echo ""
    echo "Stopping services..."
    for pid in "${PIDS[@]}"; do
      if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
      fi
    done
    wait 2>/dev/null || true
  fi
  exit "$exit_code"
}

trap cleanup INT TERM EXIT

ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://127.0.0.1:12346}"
ROLLUP_HOST_PORT="${ROLLUP_RPC_URL#*://}"
ROLLUP_HOST_PORT="${ROLLUP_HOST_PORT%%/*}"
ROLLUP_HOST="${ROLLUP_HOST_PORT%:*}"
ROLLUP_PORT="${ROLLUP_HOST_PORT##*:}"
if [[ "$ROLLUP_HOST" == "$ROLLUP_PORT" ]]; then
  ROLLUP_HOST="$ROLLUP_HOST_PORT"
  ROLLUP_PORT="12346"
fi

VERIFIER_BIND="${BIND_ADDR:-127.0.0.1:8080}"
VERIFIER_HOST="${VERIFIER_BIND%:*}"
VERIFIER_PORT="${VERIFIER_BIND##*:}"
if [[ "$VERIFIER_HOST" == "$VERIFIER_PORT" ]]; then
  VERIFIER_HOST="$VERIFIER_BIND"
  VERIFIER_PORT="8080"
fi

INDEXER_BIND="${INDEXER_BIND:-127.0.0.1:13100}"
INDEXER_HOST="${INDEXER_BIND%:*}"
INDEXER_PORT="${INDEXER_BIND##*:}"
if [[ "$INDEXER_HOST" == "$INDEXER_PORT" ]]; then
  INDEXER_HOST="$INDEXER_BIND"
  INDEXER_PORT="13100"
fi

MCP_BIND="${MCP_SERVER_BIND_ADDRESS:-0.0.0.0:4000}"
MCP_HOST="${MCP_BIND%:*}"
MCP_PORT="${MCP_BIND##*:}"
if [[ "$MCP_HOST" == "$MCP_PORT" ]]; then
  MCP_HOST="$MCP_BIND"
  MCP_PORT="4000"
fi

echo "Starting rollup..."
start_service "rollup" bash "$SCRIPT_DIR/run_rollup.sh" "${ROLLUP_ARGS[@]}"
wait_for_port "rollup" "$ROLLUP_HOST" "$ROLLUP_PORT" "${PIDS[0]}"

echo "Starting verifier..."
start_service "verifier" bash "$SCRIPT_DIR/run_verifier_service.sh"
wait_for_port "verifier" "$VERIFIER_HOST" "$VERIFIER_PORT" "${PIDS[1]}"

echo "Starting indexer..."
start_service "indexer" bash "$SCRIPT_DIR/run_indexer.sh"
wait_for_port "indexer" "$INDEXER_HOST" "$INDEXER_PORT" "${PIDS[2]}"

echo "Starting mcp..."
start_service "mcp" bash "$SCRIPT_DIR/run_mcp.sh"
wait_for_port "mcp" "$MCP_HOST" "$MCP_PORT" "${PIDS[3]}"

echo ""
echo "All services started. Press Ctrl+C to stop."
wait
