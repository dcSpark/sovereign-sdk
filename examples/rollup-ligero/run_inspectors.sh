#!/usr/bin/env bash
set -euo pipefail

# MCP Inspector instances for both MCP servers
# Runs in the background and opens browser tabs

INSPECTOR_VERSION="${INSPECTOR_VERSION:-0.16.7}"

# MCP 1 Inspector settings
MCP_1_URL="${MCP_1_URL:-http://localhost:3000/mcp/mcp}"
INSPECTOR_1_CLIENT_PORT="${INSPECTOR_1_CLIENT_PORT:-5173}"
INSPECTOR_1_SERVER_PORT="${INSPECTOR_1_SERVER_PORT:-6277}"

# MCP 2 Inspector settings
MCP_2_URL="${MCP_2_URL:-http://localhost:3001/mcp/mcp}"
INSPECTOR_2_CLIENT_PORT="${INSPECTOR_2_CLIENT_PORT:-5174}"
INSPECTOR_2_SERVER_PORT="${INSPECTOR_2_SERVER_PORT:-6278}"

PIDS=()

cleanup() {
  local exit_code=$?
  set +e
  if [ "${#PIDS[@]}" -gt 0 ]; then
    echo ""
    echo "Stopping inspectors..."
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

echo "Starting MCP Inspector 1 for $MCP_1_URL on port $INSPECTOR_1_CLIENT_PORT..."
CLIENT_PORT="$INSPECTOR_1_CLIENT_PORT" SERVER_PORT="$INSPECTOR_1_SERVER_PORT" \
  npx --yes "@modelcontextprotocol/inspector@$INSPECTOR_VERSION" --transport http --server-url "$MCP_1_URL" &
PIDS+=($!)

echo "Starting MCP Inspector 2 for $MCP_2_URL on port $INSPECTOR_2_CLIENT_PORT..."
CLIENT_PORT="$INSPECTOR_2_CLIENT_PORT" SERVER_PORT="$INSPECTOR_2_SERVER_PORT" \
  npx --yes "@modelcontextprotocol/inspector@$INSPECTOR_VERSION" --transport http --server-url "$MCP_2_URL" &
PIDS+=($!)

echo ""
echo "Inspectors running:"
echo "  - MCP 1: http://localhost:$INSPECTOR_1_CLIENT_PORT"
echo "  - MCP 2: http://localhost:$INSPECTOR_2_CLIENT_PORT"
echo ""
echo "Press Ctrl+C to stop."
wait
