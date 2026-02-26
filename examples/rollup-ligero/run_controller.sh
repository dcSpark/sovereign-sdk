#!/usr/bin/env bash
# ============================================================================
# run_controller.sh  --  Launcher for the service controller and dashboard
#
# Starts the rollup-ligero-service-controller binary (which manages all child
# services through a REST API) and optionally the rollup dashboard.
#
# Usage:
#   examples/rollup-ligero/run_controller.sh [OPTIONS]
#
# Options:
#   --skip-build       Skip cargo build (reuse existing binaries)
#   --release          Build in release mode (default)
#   --debug            Build in debug mode
#   --no-auto-start    Don't auto-start services on launch
#   --no-dashboard     Don't launch the dashboard
#   --bind <ADDR>      Controller bind address (default: 127.0.0.1:9090)
#   -h, --help         Show this help
#
# Controller REST API (http://<bind>):
#   GET  /services          List all managed services and their status
#   GET  /health            Health check all services
#   POST /start             Start all default services
#   POST /start/:service    Start a specific service
#   POST /stop              Stop all services
#   POST /stop/:service     Stop a specific service
#   POST /restart           Restart all services
#   POST /clean             Clean demo_data directory
#   POST /clean-database    Truncate all Postgres tables
#   POST /reset-tee         Reset TEE on configured upstream
#   POST /reset-replica     Authenticated replica reset (stop/clean/start)
#   GET  /logs              WebSocket log stream
#   GET  /logs/history      Recent log history (JSON)
#   GET  /stats             System stats (CPU, memory, disk)
#
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Defaults
SKIP_BUILD=0
RELEASE_MODE=1
AUTO_START=1
LAUNCH_DASHBOARD=1
CONTROLLER_BIND="127.0.0.1:9090"

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build)    SKIP_BUILD=1 ;;
    --release)       RELEASE_MODE=1 ;;
    --debug)         RELEASE_MODE=0 ;;
    --no-auto-start) AUTO_START=0 ;;
    --no-dashboard)  LAUNCH_DASHBOARD=0 ;;
    --bind)          CONTROLLER_BIND="$2"; shift ;;
    -h|--help)
      sed -n '2,/^# =====/{/^# =====/d;s/^# \?//;p}' "$0"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2; exit 1 ;;
  esac
  shift
done

CARGO_PROFILE=""
TARGET_DIR="debug"
if [ "$RELEASE_MODE" -eq 1 ]; then
  CARGO_PROFILE="--release"
  TARGET_DIR="release"
fi

CONTROLLER_BIN="$WORKSPACE_ROOT/target/$TARGET_DIR/rollup-ligero-service-controller"

# ── Colors ────────────────────────────────────────────────────────────────────

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

_step=0
print_step() { _step=$((_step+1)); echo ""; echo -e "${CYAN}${BOLD}[$_step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

# ── Environment ───────────────────────────────────────────────────────────────

export SERVICE_CONTROLLER_BIND="$CONTROLLER_BIND"
if [ "$AUTO_START" -eq 1 ]; then
  export SERVICE_CONTROLLER_AUTO_START=1
fi

# ── Known binaries for stale process cleanup ──────────────────────────────────

if [ "${START_REPLICA:-0}" = "1" ]; then
  KNOWN_BINARIES=(
    rollup-ligero-service-controller
  )
else
  KNOWN_BINARIES=(
    rollup-ligero-service-controller
    sov-rollup-ligero
    proof-verifier
    sov-indexer
    mcp-external
    midnight-proof-pool-service
    sov-metrics-api
    oracle
    midnight-fvk-service
  )
fi

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
  if pgrep -f "vite.*rollup-dashboard" >/dev/null 2>&1; then
    pkill "-${signal}" -f "vite.*rollup-dashboard" 2>/dev/null || true
    print_ok "Sent SIG${signal} to dashboard (vite)"
    KILLED_COUNT=$((KILLED_COUNT+1))
  fi
}

# ── Exit trap ─────────────────────────────────────────────────────────────────

DASHBOARD_PID=""

cleanup() {
  trap - EXIT INT TERM
  echo ""
  echo -e "${CYAN}${BOLD}Shutting down...${NC}"

  if [ -n "$DASHBOARD_PID" ] && kill -0 "$DASHBOARD_PID" 2>/dev/null; then
    print_info "Stopping dashboard (PID $DASHBOARD_PID)..."
    kill "$DASHBOARD_PID" 2>/dev/null || true
    wait "$DASHBOARD_PID" 2>/dev/null || true
  fi

  kill_known_processes TERM
  if [ "$KILLED_COUNT" -gt 0 ]; then
    sleep 1
    still_alive=0
    for name in "${KNOWN_BINARIES[@]}"; do
      if pgrep -x "$name" >/dev/null 2>&1; then still_alive=1; break; fi
    done
    if [ "$still_alive" -eq 1 ]; then
      print_info "Force-killing remaining processes..."
      kill_known_processes KILL
    fi
  fi

  print_ok "All processes stopped"
}
trap cleanup EXIT INT TERM

# ═══════════════════════════════════════════════════════════════════════════════

echo ""
echo -e "${BOLD}Rollup Ligero Service Controller${NC}"
echo "═══════════════════════════════════"
echo ""
echo "  Workspace:          $WORKSPACE_ROOT"
echo "  Controller bind:    $CONTROLLER_BIND"
echo "  Auto-start:         $([ "$AUTO_START" -eq 1 ] && echo yes || echo no)"
echo "  Dashboard:          $([ "$LAUNCH_DASHBOARD" -eq 1 ] && echo yes || echo no)"
echo "  Release mode:       $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"

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

# ── Step 2: Build ─────────────────────────────────────────────────────────────

if [ "$SKIP_BUILD" -eq 0 ]; then
  print_step "Building service controller and dependencies"

  print_info "Building rollup-ligero (rollup binary)..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE -p sov-rollup-ligero 2>&1 | tail -3
  print_ok "sov-rollup-ligero"

  if [ "${START_REPLICA:-0}" != "1" ]; then
    print_info "Building proof-verifier-service (worker binary)..."
    cargo build $CARGO_PROFILE -p sov-proof-verifier-service 2>&1 | tail -3
    print_ok "proof-verifier"

    print_info "Building indexer..."
    cargo build $CARGO_PROFILE -p sov-indexer 2>&1 | tail -3
    print_ok "sov-indexer"

    print_info "Building proof pool..."
    cargo build $CARGO_PROFILE -p midnight-proof-pool-service 2>&1 | tail -3
    print_ok "midnight-proof-pool-service"

    print_info "Building MCP..."
    cargo build $CARGO_PROFILE -p mcp-external 2>&1 | tail -3
    print_ok "mcp-external"

    print_info "Building metrics API..."
    cargo build $CARGO_PROFILE -p sov-metrics-api 2>&1 | tail -3
    print_ok "sov-metrics-api"
  else
    print_info "Replica mode -- skipping primary service builds"
  fi

  print_info "Building service controller..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE -p sov-rollup-ligero --bin rollup-ligero-service-controller 2>&1 | tail -3
  print_ok "rollup-ligero-service-controller"
else
  print_step "Build skipped (--skip-build)"
  if [ ! -f "$CONTROLLER_BIN" ]; then
    echo -e "  ${RED}ERROR${NC} Missing $CONTROLLER_BIN"
    exit 1
  fi
  print_ok "Binary present"
fi

# ── Step 3: Verify service scripts ────────────────────────────────────────────

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

# ── Step 5: Dashboard ────────────────────────────────────────────────────────

DASHBOARD_DIR="$SCRIPT_DIR/utils/rollup-dashboard"

if [ "$LAUNCH_DASHBOARD" -eq 1 ]; then
  print_step "Starting dashboard"

  if ! command -v node >/dev/null 2>&1; then
    echo -e "  ${YELLOW}WARN${NC} node not found -- skipping dashboard"
    LAUNCH_DASHBOARD=0
  elif ! command -v npm >/dev/null 2>&1; then
    echo -e "  ${YELLOW}WARN${NC} npm not found -- skipping dashboard"
    LAUNCH_DASHBOARD=0
  elif [ ! -d "$DASHBOARD_DIR" ]; then
    echo -e "  ${YELLOW}WARN${NC} Dashboard directory not found -- skipping"
    LAUNCH_DASHBOARD=0
  fi
fi

if [ "$LAUNCH_DASHBOARD" -eq 1 ]; then
  if [ ! -d "$DASHBOARD_DIR/node_modules" ]; then
    print_info "Installing dashboard dependencies..."
    (cd "$DASHBOARD_DIR" && npm install --no-fund --no-audit) 2>&1 | tail -3
    print_ok "npm install"
  else
    print_ok "node_modules present"
  fi

  VITE_API_TARGET="http://$CONTROLLER_BIND" npm run dev --prefix "$DASHBOARD_DIR" &
  DASHBOARD_PID=$!
  print_ok "Dashboard running (PID $DASHBOARD_PID) -- http://localhost:3333"
fi

# ── Step 6: Launch service controller ─────────────────────────────────────────

print_step "Starting service controller"

echo ""
echo -e "  ${BOLD}Controller API:${NC}"
echo "    http://$CONTROLLER_BIND/services         -- list services"
echo "    http://$CONTROLLER_BIND/health            -- health check"
echo "    http://$CONTROLLER_BIND/start             -- start all"
echo "    http://$CONTROLLER_BIND/stop              -- stop all"
echo "    http://$CONTROLLER_BIND/restart           -- restart all"
echo "    http://$CONTROLLER_BIND/clean             -- clean demo data"
echo "    http://$CONTROLLER_BIND/clean-database    -- truncate Postgres tables"
echo "    http://$CONTROLLER_BIND/reset-replica     -- reset remote replica (auth required)"
echo "    ws://$CONTROLLER_BIND/logs                -- live log stream"
if [ "$LAUNCH_DASHBOARD" -eq 1 ]; then
echo -e "  ${BOLD}Dashboard:${NC}"
echo "    http://localhost:3333"
fi
echo ""
echo "────────────────────────────────────────────────────"
echo ""

exec "$CONTROLLER_BIN"
