#!/usr/bin/env bash
# ============================================================================
# run_controller.sh  --  Unified launcher for the Nightstream service controller
#
# Starts the rollup-nightstream-service-controller binary, which manages all
# child services (rollup, worker/verifier, indexer, etc.) through a REST API.
#
# All environment variables are defined in one place below. Services that are
# not available yet in rollup-nightstream are marked as remote (disabled).
#
# Usage:
#   examples/rollup-nightstream/run_controller.sh [OPTIONS]
#
# Options:
#   --skip-build       Skip cargo build (reuse existing binaries)
#   --release          Build in release mode (default)
#   --debug            Build in debug mode
#   --no-auto-start    Don't auto-start services on launch
#   --no-dashboard     Don't launch the controller dashboard
#   --bind <ADDR>      Controller bind address (default: 127.0.0.1:9090)
#   -h, --help         Show this help
#
# The controller exposes a REST API on http://<bind>:
#   GET  /services          List all managed services and their status
#   GET  /health            Health check all services
#   POST /start             Start all default services
#   POST /start/:service    Start a specific service
#   POST /stop              Stop all services
#   POST /stop/:service     Stop a specific service
#   POST /restart           Restart all services
#   POST /build             Build all managed service binaries
#   POST /build/:service    Build a specific service binary
#   POST /clean             Clean demo_data directory
#   POST /clean-database    Truncate all Postgres tables
#   GET  /logs              WebSocket log stream
#   GET  /logs/history      Recent log history (JSON)
#   GET  /stats             System stats (CPU, memory, disk)
#
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

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

CONTROLLER_BIN="$REPO_ROOT/target/$TARGET_DIR/rollup-nightstream-service-controller"

# ═══════════════════════════════════════════════════════════════════════════════
# ENVIRONMENT VARIABLES
#
# All service configuration is centralised here. Override any variable by
# exporting it before running this script.
# ═══════════════════════════════════════════════════════════════════════════════

# ── Database ──────────────────────────────────────────────────────────────────
# PostgreSQL connection string shared by the rollup, worker, and any service
# that reads/writes worker_verified_transactions.
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-postgresql://admin:1234@localhost:5432/da}"

# ── Rollup ────────────────────────────────────────────────────────────────────
export ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://127.0.0.1:12346}"
export CHAIN_ID="${CHAIN_ID:-4321}"

# ── Worker (proof-verifier-service) ───────────────────────────────────────────
export BIND_ADDR="${BIND_ADDR:-127.0.0.1:8080}"
export LOG_LEVEL="${LOG_LEVEL:-info}"
export RUST_LOG="${RUST_LOG:-info,midnight_privacy=debug,sov_nightstream_adapter=debug}"
# export METHOD_ID=              # auto-computed from value_validator ROM if unset
# export MIDNIGHT_METHOD_ID=     # auto-computed from Nightstream ROM if unset
# export MAX_CONCURRENT=         # defaults to number of CPUs
# export DEFER_SUBMISSION=false  # set to true to queue txs

# ── Indexer ───────────────────────────────────────────────────────────────────
export INDEX_DB="${INDEX_DB:-postgresql://admin:1234@localhost:5432/indexer}"

# ── Proof Pool ────────────────────────────────────────────────────────────────
export PROOF_POOL_BIND_ADDR="${PROOF_POOL_BIND_ADDR:-127.0.0.1:11235}"
export MAX_PROOFS="${MAX_PROOFS:-1}"
export MAX_CONCURRENT_PROOFS="${MAX_CONCURRENT_PROOFS:-5}"
export ADMIN_WALLET_PRIVATE_KEY="${ADMIN_WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"

# ── MCP ───────────────────────────────────────────────────────────────────────
export MCP_SERVER_BIND_ADDRESS="${MCP_SERVER_BIND_ADDRESS:-0.0.0.0:3000}"
export WALLET_PRIVATE_KEY="${WALLET_PRIVATE_KEY:-75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd}"
# export MCP_SESSION_DB_URL=     # Postgres URL for MCP session persistence (optional)

# ── Metrics ───────────────────────────────────────────────────────────────────
export METRICS_API_BIND="${METRICS_API_BIND:-0.0.0.0:13200}"

# ── FVK (optional — only starts when POOL_FVK_PK is set) ─────────────────────
# export POOL_FVK_PK=            # set to enable FVK service + viewer-commitment enforcement
export MIDNIGHT_FVK_SERVICE_DB="${MIDNIGHT_FVK_SERVICE_DB:-postgresql://admin:1234@localhost:5432/fvk}"

# ── Oracle (optional — only starts when START_ORACLE=1) ──────────────────────
# export START_ORACLE=1
export ORACLE_SIGNING_KEY_HEX="${ORACLE_SIGNING_KEY_HEX:-0x26515ce9a1569fd28569e82a6aef29049d7197a79913d27f871a6ade48563354}"
export TEE_ORACLE_PUBKEY_HEX="${TEE_ORACLE_PUBKEY_HEX:-0x2e7a268b5b68ef23fd64ebfcfcf3b41b6ab74643051dcac82d724dac2091cf4d}"

# ── Service controller ────────────────────────────────────────────────────────
export SERVICE_CONTROLLER_BIND="$CONTROLLER_BIND"
export SERVICE_TARGET_DIR="$TARGET_DIR"
export SERVICE_CONTROLLER_BUILD_MODE="$([ "$RELEASE_MODE" -eq 1 ] && echo release || echo debug)"
if [ "$AUTO_START" -eq 1 ]; then
  export SERVICE_CONTROLLER_AUTO_START=1
fi

# ═══════════════════════════════════════════════════════════════════════════════

# Colors
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

step=0
print_step() { step=$((step+1)); echo ""; echo -e "${CYAN}${BOLD}[$step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

KNOWN_BINARIES=(
  rollup-nightstream-service-controller
  sov-rollup-nightstream
  proof-verifier
  sov-indexer
  midnight-proof-pool-service
  mcp-external
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
  if pgrep -f "vite.*rollup-dashboard" >/dev/null 2>&1; then
    pkill "-${signal}" -f "vite.*rollup-dashboard" 2>/dev/null || true
    print_ok "Sent SIG${signal} to dashboard (vite)"
    KILLED_COUNT=$((KILLED_COUNT+1))
  fi
}

echo ""
echo -e "${BOLD}Nightstream Service Controller${NC}"
echo "═══════════════════════════════"
echo ""
echo "  Repo root:          $REPO_ROOT"
echo "  Controller bind:    $CONTROLLER_BIND"
echo "  Auto-start:         $([ "$AUTO_START" -eq 1 ] && echo yes || echo no)"
echo "  Dashboard:          $([ "$LAUNCH_DASHBOARD" -eq 1 ] && echo yes || echo no)"
echo "  Release mode:       $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"
echo "  DA connection:      $DA_CONNECTION_STRING"
echo "  Rollup RPC:         $ROLLUP_RPC_URL"
echo "  Worker bind:        $BIND_ADDR"
echo "  Indexer DB:         $INDEX_DB"
echo "  Proof pool bind:    $PROOF_POOL_BIND_ADDR"
echo "  MCP bind:           $MCP_SERVER_BIND_ADDRESS"
echo "  Metrics bind:       $METRICS_API_BIND"

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

  print_info "Building rollup-nightstream (rollup binary)..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE -p sov-rollup-nightstream 2>&1 | tail -3
  print_ok "sov-rollup-nightstream"

  print_info "Building proof-verifier-service (worker binary)..."
  cargo build $CARGO_PROFILE -p sov-proof-verifier-service 2>&1 | tail -3
  print_ok "proof-verifier"

  print_info "Building generate_proof_tx (genesis helper)..."
  cargo build $CARGO_PROFILE -p sov-nightstream-adapter --example generate_proof_tx --features native 2>&1 | tail -3
  print_ok "generate_proof_tx"

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

  print_info "Building oracle..."
  cargo build $CARGO_PROFILE -p oracle 2>&1 | tail -3
  print_ok "oracle"

  print_info "Building FVK service..."
  cargo build $CARGO_PROFILE -p midnight-fvk-service 2>&1 | tail -3
  print_ok "midnight-fvk-service"

  print_info "Building service controller..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE -p sov-rollup-nightstream --bin rollup-nightstream-service-controller 2>&1 | tail -3
  print_ok "rollup-nightstream-service-controller"
else
  print_step "Build skipped (--skip-build)"
  [ -f "$CONTROLLER_BIN" ] || { echo "  Missing $CONTROLLER_BIN"; exit 1; }
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

for script_name in "${REQUIRED_SCRIPTS[@]}"; do
  if [ -f "$SCRIPT_DIR/$script_name" ]; then
    print_ok "$script_name"
  else
    echo -e "  ${RED}MISSING${NC} $script_name"
    echo "  The service controller requires all service scripts to exist."
    exit 1
  fi
done

# ── Step 4: Seed bridge assets ────────────────────────────────────────────────

print_step "Seeding Midnight bridge assets"

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
  print_info "No assets/ directory — bridge asset seeding skipped"
fi

# ── Exit trap: kill everything on shutdown ────────────────────────────────────

DASHBOARD_DIR="$SCRIPT_DIR/utils/rollup-dashboard"
DASHBOARD_PID=""

cleanup() {
  trap - EXIT INT TERM
  echo ""
  echo -e "${CYAN}${BOLD}Shutting down...${NC}"

  if [ -n "$DASHBOARD_PID" ] && kill -0 "$DASHBOARD_PID" 2>/dev/null; then
    echo -e "  ${YELLOW}INFO${NC} Stopping dashboard (PID $DASHBOARD_PID)..."
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
      echo -e "  ${YELLOW}INFO${NC} Force-killing remaining processes..."
      kill_known_processes KILL
    fi
  fi

  echo -e "  ${GREEN}OK${NC} All processes stopped"
}
trap cleanup EXIT INT TERM

# ── Step 5: Dashboard ────────────────────────────────────────────────────────

if [ "$LAUNCH_DASHBOARD" -eq 1 ]; then
  print_step "Starting controller dashboard"

  if ! command -v node >/dev/null 2>&1; then
    echo -e "  ${YELLOW}WARN${NC} node not found — skipping dashboard"
    LAUNCH_DASHBOARD=0
  elif ! command -v npm >/dev/null 2>&1; then
    echo -e "  ${YELLOW}WARN${NC} npm not found — skipping dashboard"
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
  print_ok "Dashboard running (PID $DASHBOARD_PID) — http://localhost:3333"
fi

# ── Step 6: Launch controller ────────────────────────────────────────────────

print_step "Starting service controller"

echo ""
echo -e "  ${BOLD}Controller API:${NC}"
echo "    http://$CONTROLLER_BIND/services     — list services"
echo "    http://$CONTROLLER_BIND/health        — health check"
echo "    http://$CONTROLLER_BIND/start         — start all"
echo "    http://$CONTROLLER_BIND/stop          — stop all"
echo "    http://$CONTROLLER_BIND/restart       — restart all"
echo "    http://$CONTROLLER_BIND/clean-database — truncate Postgres tables"
echo "    ws://$CONTROLLER_BIND/logs            — live log stream"
if [ "$LAUNCH_DASHBOARD" -eq 1 ]; then
echo -e "  ${BOLD}Dashboard:${NC}"
echo "    http://localhost:3333"
fi
echo ""
echo "────────────────────────────────────────────────────"
echo ""

"$CONTROLLER_BIN"
