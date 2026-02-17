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
CONTROLLER_BIND="127.0.0.1:9090"

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build)    SKIP_BUILD=1 ;;
    --release)       RELEASE_MODE=1 ;;
    --debug)         RELEASE_MODE=0 ;;
    --no-auto-start) AUTO_START=0 ;;
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

# ── Disabled services (API mismatch with Nightstream /prove endpoint) ─────────
# Proof pool and MCP require witness-based /prove API updates before they can
# work with the Nightstream backend. Mark them as remote to skip auto-start.
export SERVICE_PROOF_POOL_REMOTE="${SERVICE_PROOF_POOL_REMOTE:-1}"
export SERVICE_MCP_REMOTE="${SERVICE_MCP_REMOTE:-1}"

# ── Service controller ────────────────────────────────────────────────────────
export SERVICE_CONTROLLER_BIND="$CONTROLLER_BIND"
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

echo ""
echo -e "${BOLD}Nightstream Service Controller${NC}"
echo "═══════════════════════════════"
echo ""
echo "  Repo root:          $REPO_ROOT"
echo "  Controller bind:    $CONTROLLER_BIND"
echo "  Auto-start:         $([ "$AUTO_START" -eq 1 ] && echo yes || echo no)"
echo "  Release mode:       $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"
echo "  DA connection:      $DA_CONNECTION_STRING"
echo "  Rollup RPC:         $ROLLUP_RPC_URL"
echo "  Worker bind:        $BIND_ADDR"
echo "  Indexer DB:         $INDEX_DB"
echo "  Proof pool bind:    $PROOF_POOL_BIND_ADDR"
echo "  MCP bind:           $MCP_SERVER_BIND_ADDRESS"
echo "  Metrics bind:       $METRICS_API_BIND"

# ── Step 1: Build ─────────────────────────────────────────────────────────────

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

  print_info "Building service controller..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE -p sov-rollup-nightstream --bin rollup-nightstream-service-controller 2>&1 | tail -3
  print_ok "rollup-nightstream-service-controller"
else
  print_step "Build skipped (--skip-build)"
  [ -f "$CONTROLLER_BIN" ] || { echo "  Missing $CONTROLLER_BIN"; exit 1; }
  print_ok "Binary present"
fi

# ── Step 2: Verify service scripts ────────────────────────────────────────────

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

# ── Step 3: Seed bridge assets ────────────────────────────────────────────────

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

# ── Step 4: Launch ────────────────────────────────────────────────────────────

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
echo ""
echo "────────────────────────────────────────────────────"
echo ""

exec "$CONTROLLER_BIN"
