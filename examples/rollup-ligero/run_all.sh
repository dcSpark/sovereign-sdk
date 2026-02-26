#!/usr/bin/env bash
# ============================================================================
# run_all.sh  --  Unified launcher for all rollup-ligero services
#
# Starts every service in-process (oracle, rollup, verifier, fvk, indexer,
# proof-pool, mcp, metrics) and optionally the rollup dashboard.
#
# Usage:
#   examples/rollup-ligero/run_all.sh [OPTIONS] [-- ROLLUP_ARGS...]
#
# Options:
#   --skip-build       Skip cargo build (reuse existing binaries)
#   --release          Build in release mode (default: uses whatever cargo defaults to)
#   --debug            Build in debug mode
#   -h, --help         Show this help
#
# Environment Variables (commonly overridden):
#   DA_CONNECTION_STRING            PostgreSQL/SQLite DA connection
#   ROLLUP_RPC_URL                  Rollup JSON-RPC URL (default: http://127.0.0.1:12346)
#   BIND_ADDR                       Verifier bind address (default: 127.0.0.1:8080)
#   INDEXER_BIND                    Indexer bind address (default: 127.0.0.1:13100)
#   MCP_SERVER_BIND_ADDRESS         MCP bind address (default: 0.0.0.0:3000)
#   METRICS_API_BIND                Metrics bind address (default: 0.0.0.0:13200)
#   PROOF_POOL_BIND_ADDR            Proof pool bind address (default: 127.0.0.1:11235)
#   POOL_FVK_PK                     Set to enable FVK service
#   START_ORACLE                    Set to 1 to start oracle
#
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export WORKSPACE_ROOT

# ── CLI argument parsing ──────────────────────────────────────────────────────

SKIP_BUILD=0
BUILD_MODE=""
ROLLUP_ARGS=()
PARSING_ROLLUP_ARGS=0

while [ $# -gt 0 ]; do
  if [ "$PARSING_ROLLUP_ARGS" -eq 1 ]; then
    ROLLUP_ARGS+=("$1")
    shift
    continue
  fi
  case "$1" in
    --skip-build)    SKIP_BUILD=1 ;;
    --release)       BUILD_MODE="--release" ;;
    --debug)         BUILD_MODE="" ;;
    --)              PARSING_ROLLUP_ARGS=1 ;;
    -h|--help)
      sed -n '2,/^# =====/{/^# =====/d;s/^# \?//;p}' "$0"
      exit 0
      ;;
    *)
      ROLLUP_ARGS+=("$1")
      ;;
  esac
  shift
done

source "$SCRIPT_DIR/pool_fvk_env.sh"
resolve_pool_fvk_pk
print_pool_fvk_pk_status

WAIT_TIMEOUT_SECONDS="${WAIT_TIMEOUT_SECONDS:-600}"
WAIT_SLEEP_SECONDS="${WAIT_SLEEP_SECONDS:-1}"
SHUTDOWN_GRACE_SECONDS="${SHUTDOWN_GRACE_SECONDS:-10}"
SHUTDOWN_FORCE_SECONDS="${SHUTDOWN_FORCE_SECONDS:-5}"

PIDS=()
NAMES=()


# ── Colors and pretty output ─────────────────────────────────────────────────

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

_step=0
print_step() { _step=$((_step+1)); echo ""; echo -e "${CYAN}${BOLD}[$_step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

# ── Helper functions ─────────────────────────────────────────────────────────

normalize_host() {
  local host="$1"
  if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
    echo "127.0.0.1"
    return
  fi
  echo "$host"
}

is_postgres_url() {
  local url="$1"
  [[ "$url" == postgres://* || "$url" == postgresql://* ]]
}

env_nonneg_int_or_default() {
  local key="$1"
  local default="$2"
  local value="${!key:-}"
  if [[ "$value" =~ ^[0-9]+$ ]]; then
    echo "$value"
  else
    echo "$default"
  fi
}

postgres_hostport_from_url() {
  local url="$1"
  if ! is_postgres_url "$url"; then
    return 1
  fi

  local rest="${url#*://}"
  local authority="${rest%%/*}"
  authority="${authority%%\?*}"
  authority="${authority%%#*}"
  local hostport="${authority##*@}"

  if [[ -z "$hostport" ]]; then
    return 1
  fi

  if [[ "$hostport" != *:* && "$hostport" != \[*\]* ]]; then
    hostport="${hostport}:5432"
  elif [[ "$hostport" == \[*\] && "$hostport" != *"]:*" ]]; then
    hostport="${hostport}:5432"
  fi

  echo "$hostport"
}

check_postgres_connection_budget() {
  local da_conn="$1"
  if [[ -z "$da_conn" ]]; then
    return 0
  fi

  local mode="${SOV_POSTGRES_POOL_BUDGET_MODE:-warn}"
  local mode_lc
  mode_lc="$(printf "%s" "$mode" | tr '[:upper:]' '[:lower:]')"
  if [[ "$mode_lc" == "off" ]]; then
    return 0
  fi

  if ! is_postgres_url "$da_conn"; then
    return 0
  fi

  local da_hostport
  da_hostport="$(postgres_hostport_from_url "$da_conn" 2>/dev/null || true)"

  local da_pool_max
  da_pool_max="$(env_nonneg_int_or_default "SOV_MIDNIGHT_DA_POSTGRES_MAX_CONNECTIONS" "20")"
  local worker_db_pool_max
  worker_db_pool_max="$(env_nonneg_int_or_default "SOV_WORKER_DB_POSTGRES_MAX_CONNECTIONS" "10")"
  local preferred_db_pool_max
  preferred_db_pool_max="$(env_nonneg_int_or_default "SOV_PREFERRED_DB_POSTGRES_MAX_CONNECTIONS" "10")"
  local verifier_pool_max
  verifier_pool_max="$(env_nonneg_int_or_default "SOV_PROOF_VERIFIER_POSTGRES_MAX_CONNECTIONS" "12")"
  local indexer_pool_max
  indexer_pool_max="$(env_nonneg_int_or_default "SOV_INDEXER_POSTGRES_MAX_CONNECTIONS" "20")"
  local metrics_da_pool_max
  metrics_da_pool_max="$(env_nonneg_int_or_default "SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS" "10")"
  local metrics_indexer_pool_max
  metrics_indexer_pool_max="$(env_nonneg_int_or_default "SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS" "10")"

  local proof_pool_embedded_verifiers
  proof_pool_embedded_verifiers="$(env_nonneg_int_or_default "SOV_PROOF_POOL_EMBEDDED_VERIFIER_COUNT" "2")"
  local verifier_total
  verifier_total=$((verifier_pool_max * (1 + proof_pool_embedded_verifiers)))

  local index_db_url="${INDEX_DB:-sqlite://demo_data/wallet_index.sqlite?mode=rwc}"
  local index_db_pool_max=0
  if is_postgres_url "$index_db_url"; then
    local index_db_hostport
    index_db_hostport="$(postgres_hostport_from_url "$index_db_url" 2>/dev/null || true)"
    if [[ -n "$da_hostport" && -n "$index_db_hostport" && "$index_db_hostport" == "$da_hostport" ]]; then
      index_db_pool_max="$indexer_pool_max"
    fi
  fi

  local metrics_index_db_url="${INDEXER_DB_CONNECTION_STRING:-$index_db_url}"
  local metrics_index_pool_in_budget=0
  if is_postgres_url "$metrics_index_db_url"; then
    local metrics_index_db_hostport
    metrics_index_db_hostport="$(postgres_hostport_from_url "$metrics_index_db_url" 2>/dev/null || true)"
    if [[ -n "$da_hostport" && -n "$metrics_index_db_hostport" && "$metrics_index_db_hostport" == "$da_hostport" ]]; then
      metrics_index_pool_in_budget="$metrics_indexer_pool_max"
    fi
  fi

  local extra_pool_max
  extra_pool_max="$(env_nonneg_int_or_default "SOV_POSTGRES_POOL_BUDGET_EXTRA_CONNECTIONS" "0")"

  local estimated_total
  estimated_total=$((da_pool_max + worker_db_pool_max + preferred_db_pool_max + verifier_total + indexer_pool_max + index_db_pool_max + metrics_da_pool_max + metrics_index_pool_in_budget + extra_pool_max))

  echo ""
  echo "Postgres pool budget (estimated for DA DB host)"
  echo "  Target DB: $(redact_db_url "$da_conn")"
  echo "  rollup.da_pool:               $da_pool_max (SOV_MIDNIGHT_DA_POSTGRES_MAX_CONNECTIONS)"
  echo "  rollup.worker_db_pool:        $worker_db_pool_max (SOV_WORKER_DB_POSTGRES_MAX_CONNECTIONS)"
  echo "  rollup.preferred_db_pool:     $preferred_db_pool_max (SOV_PREFERRED_DB_POSTGRES_MAX_CONNECTIONS)"
  echo "  verifier pools total:         $verifier_total (SOV_PROOF_VERIFIER_POSTGRES_MAX_CONNECTIONS x (1 + SOV_PROOF_POOL_EMBEDDED_VERIFIER_COUNT))"
  echo "  indexer.da_pool:              $indexer_pool_max (SOV_INDEXER_POSTGRES_MAX_CONNECTIONS)"
  if [[ "$index_db_pool_max" -gt 0 ]]; then
    echo "  indexer.index_pool:           $index_db_pool_max (INDEX_DB points to same Postgres host)"
  fi
  echo "  metrics.da_pool:              $metrics_da_pool_max (SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS)"
  if [[ "$metrics_index_pool_in_budget" -gt 0 ]]; then
    echo "  metrics.index_pool:           $metrics_index_pool_in_budget (SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS)"
  fi
  if [[ "$extra_pool_max" -gt 0 ]]; then
    echo "  extra_manual_pools:           $extra_pool_max (SOV_POSTGRES_POOL_BUDGET_EXTRA_CONNECTIONS)"
  fi
  echo "  -------------------------------------------------------------"
  echo "  estimated_pool_max_total:     $estimated_total"

  if ! command -v psql >/dev/null 2>&1; then
    echo "  WARN: psql not found; cannot compare against server max_connections."
    if [[ "$mode_lc" == "enforce" ]]; then
      echo "  ERROR: SOV_POSTGRES_POOL_BUDGET_MODE=enforce requires psql in PATH."
      exit 1
    fi
    return 0
  fi

  local connect_timeout_secs
  connect_timeout_secs="$(env_nonneg_int_or_default "SOV_POSTGRES_POOL_BUDGET_CONNECT_TIMEOUT_SECS" "3")"
  local settings
  settings="$(PGCONNECT_TIMEOUT="$connect_timeout_secs" psql "$da_conn" -Atqc "SELECT current_setting('max_connections'), current_setting('superuser_reserved_connections')" 2>/dev/null || true)"

  if [[ ! "$settings" =~ ^([0-9]+)\|([0-9]+)$ ]]; then
    echo "  WARN: could not query max_connections from Postgres (connection or auth issue)."
    if [[ "$mode_lc" == "enforce" ]]; then
      echo "  ERROR: budget enforcement is enabled but Postgres limits could not be read."
      exit 1
    fi
    return 0
  fi

  local max_connections="${BASH_REMATCH[1]}"
  local reserved_connections="${BASH_REMATCH[2]}"
  local usable_connections=$((max_connections - reserved_connections))
  if (( usable_connections < 0 )); then
    usable_connections=0
  fi

  local headroom
  headroom="$(env_nonneg_int_or_default "SOV_POSTGRES_POOL_BUDGET_HEADROOM" "10")"
  local safe_budget=$((usable_connections - headroom))
  if (( safe_budget < 0 )); then
    safe_budget=0
  fi

  echo "  server.max_connections:       $max_connections"
  echo "  server.reserved_connections:  $reserved_connections"
  echo "  server.usable_connections:    $usable_connections"
  echo "  required_headroom:            $headroom (SOV_POSTGRES_POOL_BUDGET_HEADROOM)"
  echo "  safe_budget:                  $safe_budget"

  if (( estimated_total > safe_budget )); then
    local over_by=$((estimated_total - safe_budget))
    echo "  STATUS: OVER BUDGET by $over_by connection(s)."
    echo "  Hint: lower one or more *_MAX_CONNECTIONS env vars or raise server max_connections."
    if [[ "$mode_lc" == "enforce" ]]; then
      echo "  ERROR: refusing to start services due to Postgres pool budget overflow."
      exit 1
    fi
  else
    local margin=$((safe_budget - estimated_total))
    echo "  STATUS: OK (margin: $margin connection(s))."
  fi
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
      print_ok "$name is listening on $host:$port"
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
  print_ok "Started $name (pid $pid)"
}

kill_tree() {
  local sig="$1"
  local pid="$2"

  if [[ -z "$pid" ]]; then
    return 0
  fi

  pkill -"$sig" -P "$pid" 2>/dev/null || true
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

KNOWN_BINARIES=(
  sov-rollup-ligero
  rollup-ligero-service-controller
  proof-verifier
  sov-indexer
  mcp-external
  midnight-proof-pool-service
  sov-metrics-api
  oracle
  midnight-fvk-service
)

kill_known_processes() {
  local signal="${1:-TERM}"
  KILLED_COUNT=0
  for name in "${KNOWN_BINARIES[@]}"; do
    if pgrep -x "$name" >/dev/null 2>&1; then
      pkill "-${signal}" -x "$name" 2>/dev/null || true
      print_ok "Sent SIG${signal} to $name"
      KILLED_COUNT=$((KILLED_COUNT+1))
    fi
  done
}

cleanup() {
  local exit_code=$?
  trap - INT TERM EXIT
  set +e

  echo ""
  echo -e "${CYAN}${BOLD}Shutting down...${NC}"

  if [ "${#PIDS[@]}" -gt 0 ]; then
    local i pid name
    for i in "${!PIDS[@]}"; do
      pid="${PIDS[$i]}"
      name="${NAMES[$i]:-service}"
      if kill -0 "$pid" 2>/dev/null; then
        print_info "$name (pid $pid): SIGTERM"
        kill_tree TERM "$pid"
      fi
    done

    if ! wait_for_pids "$SHUTDOWN_GRACE_SECONDS" "${PIDS[@]}"; then
      print_info "Timeout after ${SHUTDOWN_GRACE_SECONDS}s; sending SIGKILL..."
      for i in "${!PIDS[@]}"; do
        pid="${PIDS[$i]}"
        name="${NAMES[$i]:-service}"
        if kill -0 "$pid" 2>/dev/null; then
          print_info "$name (pid $pid): SIGKILL"
          kill_tree KILL "$pid"
        fi
      done
      wait_for_pids "$SHUTDOWN_FORCE_SECONDS" "${PIDS[@]}" || true
    fi

    for pid in "${PIDS[@]}"; do
      wait "$pid" 2>/dev/null || true
    done
  fi

  print_ok "All processes stopped"
  exit "$exit_code"
}

trap cleanup INT TERM EXIT

redact_db_url() {
  local url="$1"
  case "$url" in
    *"://"*)
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

# ═══════════════════════════════════════════════════════════════════════════════
#  PRE-FLIGHT CHECKS
# ═══════════════════════════════════════════════════════════════════════════════

echo ""
echo -e "${BOLD}Rollup Ligero Services${NC}"
echo "═══════════════════════════════"

# ── Step 1: Kill stale processes ──────────────────────────────────────────────

print_step "Killing stale processes from previous runs"

kill_known_processes TERM
if [ "$KILLED_COUNT" -gt 0 ]; then
  sleep 1
  still_alive=0
  for name in "${KNOWN_BINARIES[@]}"; do
    if pgrep -x "$name" >/dev/null 2>&1; then still_alive=1; break; fi
  done
  if [ "$still_alive" -eq 1 ]; then
    print_info "Some processes didn't exit, sending SIGKILL..."
    kill_known_processes KILL
    sleep 0.5
  fi
  print_ok "Stale processes cleaned up"
else
  print_ok "No stale processes found"
fi

# ── Step 2: Build (optional) ─────────────────────────────────────────────────

if [ "$SKIP_BUILD" -eq 0 ] && [ -n "$BUILD_MODE" ]; then
  print_step "Building binaries ($BUILD_MODE)"

  print_info "Building rollup-ligero..."
  SKIP_GUEST_BUILD=1 cargo build $BUILD_MODE -p sov-rollup-ligero 2>&1 | tail -3
  print_ok "sov-rollup-ligero"

  print_info "Building proof-verifier-service..."
  cargo build $BUILD_MODE -p sov-proof-verifier-service 2>&1 | tail -3
  print_ok "proof-verifier"

  print_info "Building indexer..."
  cargo build $BUILD_MODE -p sov-indexer 2>&1 | tail -3
  print_ok "sov-indexer"

  print_info "Building proof pool..."
  cargo build $BUILD_MODE -p midnight-proof-pool-service 2>&1 | tail -3
  print_ok "midnight-proof-pool-service"

  print_info "Building MCP..."
  cargo build $BUILD_MODE -p mcp-external 2>&1 | tail -3
  print_ok "mcp-external"

  print_info "Building metrics API..."
  cargo build $BUILD_MODE -p sov-metrics-api 2>&1 | tail -3
  print_ok "sov-metrics-api"
elif [ "$SKIP_BUILD" -eq 1 ]; then
  print_step "Build skipped (--skip-build)"
  print_ok "Reusing existing binaries"
fi

# ── Step 3: Verify service scripts ───────────────────────────────────────────

print_step "Checking service scripts"

REQUIRED_SCRIPTS=(
  run_rollup.sh
  run_verifier_service.sh
  run_oracle.sh
  run_fvk_service.sh
  run_indexer.sh
  run_proof_pool.sh
  run_mcp.sh
  run_metrics.sh
)

ALL_SCRIPTS_OK=1
for script_name in "${REQUIRED_SCRIPTS[@]}"; do
  if [ -f "$SCRIPT_DIR/$script_name" ]; then
    print_ok "$script_name"
  else
    echo -e "  ${RED}MISSING${NC} $script_name"
    ALL_SCRIPTS_OK=0
  fi
done
if [ "$ALL_SCRIPTS_OK" -eq 0 ]; then
  echo -e "  ${RED}ERROR${NC} Missing required service scripts. Cannot start."
  exit 1
fi

# ── Step 4: Seed bridge assets ────────────────────────────────────────────────

print_step "Seeding bridge assets"

ROLLUP_DATA_DIR="$SCRIPT_DIR/demo_data"
mkdir -p "$ROLLUP_DATA_DIR"

ASSETS_DIR="$SCRIPT_DIR/assets"
if [ -d "$ASSETS_DIR" ]; then
  if [ ! -f "$ROLLUP_DATA_DIR/midnight_bridge_signer.json" ] && [ -f "$ASSETS_DIR/midnight_bridge_signer.json" ]; then
    cp "$ASSETS_DIR/midnight_bridge_signer.json" "$ROLLUP_DATA_DIR/"
    print_ok "Restored $ROLLUP_DATA_DIR/midnight_bridge_signer.json"
  else
    print_ok "midnight_bridge_signer.json (present)"
  fi

  if [ ! -f "$ROLLUP_DATA_DIR/midnight_bridge_events.json" ] && [ -f "$ASSETS_DIR/midnight_bridge_events.json" ]; then
    cp "$ASSETS_DIR/midnight_bridge_events.json" "$ROLLUP_DATA_DIR/"
    print_ok "Restored $ROLLUP_DATA_DIR/midnight_bridge_events.json"
  else
    print_ok "midnight_bridge_events.json (present)"
  fi
else
  print_info "No assets/ directory -- bridge asset seeding skipped"
fi

# ═══════════════════════════════════════════════════════════════════════════════
#  CONFIGURATION
# ═══════════════════════════════════════════════════════════════════════════════

# ── Step 5: DA configuration and Postgres budget ─────────────────────────────

print_step "Validating configuration"

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
    echo -e "  ${RED}ERROR${NC}: DA_CONNECTION_STRING does not match the rollup DA connection_string!"
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

EFFECTIVE_DA_CONN="${DA_CONNECTION_STRING:-$ROLLUP_DA_CONN}"
check_postgres_connection_budget "$EFFECTIVE_DA_CONN"

# ── Resolve bind addresses ────────────────────────────────────────────────────

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

PROOF_POOL_BIND="${PROOF_POOL_BIND_ADDR:-127.0.0.1:11235}"
PROOF_POOL_HOST="${PROOF_POOL_BIND%:*}"
PROOF_POOL_PORT="${PROOF_POOL_BIND##*:}"
if [[ "$PROOF_POOL_HOST" == "$PROOF_POOL_PORT" ]]; then
  PROOF_POOL_HOST="$PROOF_POOL_BIND"
  PROOF_POOL_PORT="11235"
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
  print_info "Generated MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN for midnight-fvk-service private lookups."
fi

METRICS_HOST_NORMALIZED="$(normalize_host "$METRICS_HOST")"
export METRICS_API_URL="${METRICS_API_URL:-http://${METRICS_HOST_NORMALIZED}:${METRICS_PORT}}"

if [[ -n "${DEFER_SUBMISSION:-}" && -z "${DEFER_SEQUENCER_SUBMISSION:-}" ]]; then
  export DEFER_SEQUENCER_SUBMISSION="$DEFER_SUBMISSION"
fi
if [[ -n "${DEFER_SEQUENCER_SUBMISSION:-}" ]]; then
  export DEFER_SEQUENCER_SUBMISSION
  print_info "DEFER_SEQUENCER_SUBMISSION=$DEFER_SEQUENCER_SUBMISSION (verifier will defer sequencer submission)"
fi

# ── Print configuration summary ───────────────────────────────────────────────

echo ""
echo -e "  ${BOLD}Service endpoints:${NC}"
echo "    Rollup:        $ROLLUP_RPC_URL"
echo "    Verifier:      http://$(normalize_host "$VERIFIER_HOST"):$VERIFIER_PORT"
echo "    Indexer:       http://$(normalize_host "$INDEXER_HOST"):$INDEXER_PORT"
echo "    MCP:           http://$(normalize_host "$MCP_HOST"):$MCP_PORT"
echo "    Metrics:       http://$(normalize_host "$METRICS_HOST"):$METRICS_PORT"
echo "    Proof Pool:    http://$(normalize_host "$PROOF_POOL_HOST"):$PROOF_POOL_PORT"
if [[ -n "${POOL_FVK_PK:-}" ]]; then
echo "    FVK:           http://$(normalize_host "$FVK_HOST"):$FVK_PORT"
fi
if [[ -n "${START_ORACLE:-}" ]]; then
echo "    Oracle:        http://$(normalize_host "$ORACLE_HOST"):$ORACLE_PORT"
fi

# ═══════════════════════════════════════════════════════════════════════════════
#  START SERVICES
# ═══════════════════════════════════════════════════════════════════════════════

print_step "Starting services"

if [[ -n "${START_ORACLE:-}" ]]; then
  print_info "Starting oracle..."
  start_service "oracle" bash "$SCRIPT_DIR/run_oracle.sh"
  wait_for_port "oracle" "$ORACLE_HOST" "$ORACLE_PORT" "$LAST_PID"
fi

print_info "Starting rollup..."
start_service "rollup" bash "$SCRIPT_DIR/run_rollup.sh" ${ROLLUP_ARGS[@]+"${ROLLUP_ARGS[@]}"}
wait_for_port "rollup" "$ROLLUP_HOST" "$ROLLUP_PORT" "$LAST_PID"

print_info "Starting verifier..."
start_service "verifier" bash "$SCRIPT_DIR/run_verifier_service.sh"
wait_for_port "verifier" "$VERIFIER_HOST" "$VERIFIER_PORT" "$LAST_PID"

if [[ -n "${POOL_FVK_PK:-}" ]]; then
  print_info "Starting midnight-fvk-service..."
  start_service "fvk-service" bash "$SCRIPT_DIR/run_fvk_service.sh"
  wait_for_port "fvk-service" "$FVK_HOST" "$FVK_PORT" "$LAST_PID"
fi

print_info "Starting indexer..."
start_service "indexer" bash "$SCRIPT_DIR/run_indexer.sh"
wait_for_port "indexer" "$INDEXER_HOST" "$INDEXER_PORT" "$LAST_PID"

print_info "Starting proof pool..."
start_service "proof-pool" bash "$SCRIPT_DIR/run_proof_pool.sh"
wait_for_port "proof-pool" "$PROOF_POOL_HOST" "$PROOF_POOL_PORT" "$LAST_PID"

print_info "Starting mcp..."
start_service "mcp" bash "$SCRIPT_DIR/run_mcp.sh"
wait_for_port "mcp" "$MCP_HOST" "$MCP_PORT" "$LAST_PID"

print_info "Starting metrics..."
start_service "metrics" bash "$SCRIPT_DIR/run_metrics.sh"
wait_for_port "metrics" "$METRICS_HOST" "$METRICS_PORT" "$LAST_PID"

echo ""
echo -e "${GREEN}${BOLD}All services started.${NC} Press Ctrl+C to stop."
echo ""
wait
