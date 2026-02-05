#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export WORKSPACE_ROOT

ROLLUP_ARGS=("$@")

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk
print_pool_fvk_pk_status

WAIT_TIMEOUT_SECONDS="${WAIT_TIMEOUT_SECONDS:-600}"
WAIT_SLEEP_SECONDS="${WAIT_SLEEP_SECONDS:-1}"
SHUTDOWN_GRACE_SECONDS="${SHUTDOWN_GRACE_SECONDS:-10}"
SHUTDOWN_FORCE_SECONDS="${SHUTDOWN_FORCE_SECONDS:-5}"

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
  LAST_PID="$pid"
  PIDS+=("$pid")
  NAMES+=("$name")
  echo "Started $name (pid $pid)"
}

kill_tree() {
  local sig="$1"
  local pid="$2"

  if [[ -z "$pid" ]]; then
    return 0
  fi

  # Kill children first (works on macOS and Linux)
  pkill -"$sig" -P "$pid" 2>/dev/null || true
  # Then kill the parent
  kill "-$sig" "$pid" 2>/dev/null || true
}

wait_for_pids() {
  local timeout_seconds="$1"
  shift

  local deadline=$((SECONDS + timeout_seconds))
  while (( SECONDS < deadline )); do
    local any_alive=0
    local pid
    for pid in "$@"; do
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        any_alive=1
        break
      fi
    done
    if [[ "$any_alive" -eq 0 ]]; then
      return 0
    fi
    sleep 1
  done

  return 1
}

cleanup() {
  local exit_code=$?
  trap - INT TERM EXIT
  set +e
  if [ "${#PIDS[@]}" -gt 0 ]; then
    echo ""
    echo "Stopping services..."

    local i pid name
    for i in "${!PIDS[@]}"; do
      pid="${PIDS[$i]}"
      name="${NAMES[$i]:-service}"
      if kill -0 "$pid" 2>/dev/null; then
        echo "  - $name (pid $pid): SIGTERM"
        kill_tree TERM "$pid"
      fi
    done

    if ! wait_for_pids "$SHUTDOWN_GRACE_SECONDS" "${PIDS[@]}"; then
      echo "  - timeout after ${SHUTDOWN_GRACE_SECONDS}s; sending SIGKILL..."
      for i in "${!PIDS[@]}"; do
        pid="${PIDS[$i]}"
        name="${NAMES[$i]:-service}"
        if kill -0 "$pid" 2>/dev/null; then
          echo "    - $name (pid $pid): SIGKILL"
          kill_tree KILL "$pid"
        fi
      done
      wait_for_pids "$SHUTDOWN_FORCE_SECONDS" "${PIDS[@]}" || true
    fi

    # Best-effort reap (won't block if already reparented).
    for pid in "${PIDS[@]}"; do
      wait "$pid" 2>/dev/null || true
    done
  fi
  exit "$exit_code"
}

trap cleanup INT TERM EXIT

redact_db_url() {
  local url="$1"
  case "$url" in
    *"://"*)
      # Best-effort redaction of `user:pass@host` in connection strings.
      # Only redact if the `@` appears before the first `/` or `?`.
      local prefix="${url%%://*}://"
      local rest="${url#*://}"
      local at="${rest%%@*}"
      local after_at="${rest#*@}"
      if [[ "$rest" == "$after_at" ]]; then
        echo "$url"
        return 0
      fi
      local end_userinfo="${rest%%[/?]*}"
      if [[ "${#at}" -gt "${#end_userinfo}" ]]; then
        echo "$url"
        return 0
      fi
      case "$at" in
        *:*)
          local user="${at%%:*}"
          echo "${prefix}${user}:***@${after_at}"
          return 0
          ;;
      esac
      ;;
  esac
  echo "$url"
}

extract_rollup_config_path() {
  local default_path="$SCRIPT_DIR/rollup_config.toml"
  local path="${ROLLUP_CONFIG_PATH:-$default_path}"

  local i=0
  while (( i < ${#ROLLUP_ARGS[@]} )); do
    case "${ROLLUP_ARGS[$i]}" in
      --rollup-config-path)
        if (( i + 1 < ${#ROLLUP_ARGS[@]} )); then
          path="${ROLLUP_ARGS[$((i+1))]}"
        fi
        break
        ;;
      --rollup-config-path=*)
        path="${ROLLUP_ARGS[$i]#*=}"
        break
        ;;
    esac
    i=$((i + 1))
  done

  echo "$path"
}

extract_da_connection_string_from_config() {
  local path="$1"
  if [[ -z "$path" || ! -f "$path" ]]; then
    return 0
  fi

  awk '
    BEGIN { in_da = 0 }
    /^[[:space:]]*\[da\][[:space:]]*$/ { in_da = 1; next }
    in_da && /^[[:space:]]*\[/ { in_da = 0 }
    in_da && /^[[:space:]]*connection_string[[:space:]]*=/ {
      sub(/^[[:space:]]*connection_string[[:space:]]*=[[:space:]]*/, "", $0)
      sub(/[[:space:]]*#.*/, "", $0)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
      if ($0 ~ /^".*"$/) { sub(/^"/, "", $0); sub(/"$/, "", $0) }
      print $0
      exit
    }
  ' "$path"
}

# Helpful diagnostics for common "worker tx not found" failures.
ROLLUP_CFG_PATH="$(extract_rollup_config_path)"
ROLLUP_DA_CONN="$(extract_da_connection_string_from_config "$ROLLUP_CFG_PATH")"
if [[ -n "${DA_CONNECTION_STRING:-}" || -n "$ROLLUP_DA_CONN" ]]; then
  echo ""
  echo "DA DB configuration"
  echo "  Rollup config path: ${ROLLUP_CFG_PATH}"
  if [[ -n "$ROLLUP_DA_CONN" ]]; then
    echo "  Rollup [da].connection_string: $(redact_db_url "$ROLLUP_DA_CONN")"
  fi
  if [[ -n "${DA_CONNECTION_STRING:-}" ]]; then
    echo "  DA_CONNECTION_STRING env:      $(redact_db_url "$DA_CONNECTION_STRING")"
  fi
  if [[ -n "${DA_CONNECTION_STRING:-}" && -n "$ROLLUP_DA_CONN" && "${DA_CONNECTION_STRING}" != "${ROLLUP_DA_CONN}" ]]; then
    echo ""
    echo "  ERROR: DA_CONNECTION_STRING does not match the rollup DA connection_string!"
    echo "         DA_CONNECTION_STRING env:          $(redact_db_url "$DA_CONNECTION_STRING")"
    echo "         Rollup [da].connection_string:     $(redact_db_url "$ROLLUP_DA_CONN")"
    echo ""
    echo "         The verifier/proof-pool must write worker_txs into the SAME DB the rollup/sequencer reads."
    echo "         Mismatches cause HTTP 404 'Worker transaction ... not found' on /sequencer/worker_txs/<hash>."
    echo ""
    echo "         Fix: update rollup_config.toml [da].connection_string to match DA_CONNECTION_STRING,"
    echo "         or remove DA_CONNECTION_STRING so both use the value from rollup_config.toml."
    exit 1
  fi
  echo ""
fi

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

MCP_BIND="${MCP_SERVER_BIND_ADDRESS:-0.0.0.0:3000}"
MCP_HOST="${MCP_BIND%:*}"
MCP_PORT="${MCP_BIND##*:}"
if [[ "$MCP_HOST" == "$MCP_PORT" ]]; then
  MCP_HOST="$MCP_BIND"
  MCP_PORT="3000"
fi

METRICS_BIND="${METRICS_API_BIND:-0.0.0.0:13200}"
METRICS_HOST="${METRICS_BIND%:*}"
METRICS_PORT="${METRICS_BIND##*:}"
if [[ "$METRICS_HOST" == "$METRICS_PORT" ]]; then
  METRICS_HOST="$METRICS_BIND"
  METRICS_PORT="13200"
fi

ORACLE_BIND="${ORACLE_SERVER_BIND_ADDRESS:-127.0.0.1:8090}"
ORACLE_HOST="${ORACLE_BIND%:*}"
ORACLE_PORT="${ORACLE_BIND##*:}"
if [[ "$ORACLE_HOST" == "$ORACLE_PORT" ]]; then
  ORACLE_HOST="$ORACLE_BIND"
  ORACLE_PORT="8090"
fi

FVK_BIND="${MIDNIGHT_FVK_SERVICE_BIND:-}"
if [[ -n "$FVK_BIND" ]]; then
  FVK_HOST="${FVK_BIND%:*}"
  FVK_PORT="${FVK_BIND##*:}"
  if [[ "$FVK_HOST" == "$FVK_PORT" ]]; then
    FVK_HOST="$FVK_BIND"
    FVK_PORT="8088"
  fi
else
  MIDNIGHT_FVK_SERVICE_URL="${MIDNIGHT_FVK_SERVICE_URL:-http://127.0.0.1:8088}"
  FVK_HOST_PORT="${MIDNIGHT_FVK_SERVICE_URL#*://}"
  FVK_HOST_PORT="${FVK_HOST_PORT%%/*}"
  FVK_HOST="${FVK_HOST_PORT%:*}"
  FVK_PORT="${FVK_HOST_PORT##*:}"
  if [[ "$FVK_HOST" == "$FVK_PORT" ]]; then
    FVK_HOST="$FVK_HOST_PORT"
    FVK_PORT="8088"
  fi
  export MIDNIGHT_FVK_SERVICE_BIND="$FVK_HOST:$FVK_PORT"
fi
if [[ -z "${MIDNIGHT_FVK_SERVICE_URL:-}" ]]; then
  export MIDNIGHT_FVK_SERVICE_URL="http://$FVK_HOST:$FVK_PORT"
fi

if [[ -n "${POOL_FVK_PK:-}" && -z "${MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN:-}" ]]; then
  if command -v openssl >/dev/null 2>&1; then
    export MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN="$(openssl rand -hex 16)"
  else
    export MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN="$(LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 32)"
  fi
  echo "Generated MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN for midnight-fvk-service private lookups."
fi

# Authority API configuration for mcp-external /authority/* endpoints
# Uses MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN for /authority/freeze and /authority/thaw endpoints
# Set METRICS_API_URL to point to the metrics service (enables /authority/tps)
METRICS_HOST_NORMALIZED="$(normalize_host "$METRICS_HOST")"
export METRICS_API_URL="${METRICS_API_URL:-http://${METRICS_HOST_NORMALIZED}:${METRICS_PORT}}"

PROVER_BIND="${PROVER_BIND_ADDR:-0.0.0.0:1313}"
PROVER_HOST="${PROVER_BIND%:*}"
PROVER_PORT="${PROVER_BIND##*:}"
if [[ "$PROVER_HOST" == "$PROVER_PORT" ]]; then
  PROVER_HOST="$PROVER_BIND"
  PROVER_PORT="1313"
fi

PROOF_POOL_BIND="${PROOF_POOL_BIND_ADDR:-127.0.0.1:11235}"
PROOF_POOL_HOST="${PROOF_POOL_BIND%:*}"
PROOF_POOL_PORT="${PROOF_POOL_BIND##*:}"
if [[ "$PROOF_POOL_HOST" == "$PROOF_POOL_PORT" ]]; then
  PROOF_POOL_HOST="$PROOF_POOL_BIND"
  PROOF_POOL_PORT="11235"
fi

# Support both DEFER_SUBMISSION and DEFER_SEQUENCER_SUBMISSION
if [[ -n "${DEFER_SUBMISSION:-}" && -z "${DEFER_SEQUENCER_SUBMISSION:-}" ]]; then
  export DEFER_SEQUENCER_SUBMISSION="$DEFER_SUBMISSION"
fi
if [[ -n "${DEFER_SEQUENCER_SUBMISSION:-}" ]]; then
  export DEFER_SEQUENCER_SUBMISSION
  echo "DEFER_SEQUENCER_SUBMISSION=$DEFER_SEQUENCER_SUBMISSION (verifier will defer sequencer submission)"
fi

if [[ -n "${START_ORACLE:-}" ]]; then
  echo "Starting oracle..."
  start_service "oracle" bash "$SCRIPT_DIR/run_oracle.sh"
  wait_for_port "oracle" "$ORACLE_HOST" "$ORACLE_PORT" "$LAST_PID"
fi

echo "Starting rollup..."
start_service "rollup" bash "$SCRIPT_DIR/run_rollup.sh" ${ROLLUP_ARGS[@]+"${ROLLUP_ARGS[@]}"}
wait_for_port "rollup" "$ROLLUP_HOST" "$ROLLUP_PORT" "$LAST_PID"

echo "Starting verifier..."
start_service "verifier" bash "$SCRIPT_DIR/run_verifier_service.sh"
wait_for_port "verifier" "$VERIFIER_HOST" "$VERIFIER_PORT" "$LAST_PID"

if [[ -n "${POOL_FVK_PK:-}" ]]; then
  echo "Starting midnight-fvk-service..."
  start_service "fvk-service" bash "$SCRIPT_DIR/run_fvk_service.sh"
  wait_for_port "fvk-service" "$FVK_HOST" "$FVK_PORT" "$LAST_PID"
fi

echo "Starting indexer..."
start_service "indexer" bash "$SCRIPT_DIR/run_indexer.sh"
wait_for_port "indexer" "$INDEXER_HOST" "$INDEXER_PORT" "$LAST_PID"

echo "Starting mcp..."
start_service "mcp" bash "$SCRIPT_DIR/run_mcp.sh"
wait_for_port "mcp" "$MCP_HOST" "$MCP_PORT" "$LAST_PID"

echo "Starting prover..."
start_service "prover" bash "$SCRIPT_DIR/run_prover.sh"
wait_for_port "prover" "$PROVER_HOST" "$PROVER_PORT" "$LAST_PID"

echo "Starting proof-pool..."
start_service "proof-pool" bash "$SCRIPT_DIR/run_proof_pool.sh"
wait_for_port "proof-pool" "$PROOF_POOL_HOST" "$PROOF_POOL_PORT" "$LAST_PID"

echo "Starting metrics..."
start_service "metrics" bash "$SCRIPT_DIR/run_metrics.sh"
wait_for_port "metrics" "$METRICS_HOST" "$METRICS_PORT" "$LAST_PID"

echo ""
echo "All services started. Press Ctrl+C to stop."
wait
