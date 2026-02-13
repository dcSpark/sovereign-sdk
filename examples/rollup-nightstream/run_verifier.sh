#!/usr/bin/env bash
# ============================================================================
# run_verifier.sh  --  Start the proof-verifier-service for Nightstream
#
# Shares the same SQLite DA database as the rollup so the sequencer can look
# up worker_verified_transactions written by this service.
#
# Usage:
#   examples/rollup-nightstream/run_verifier.sh [OPTIONS]
#
# Options:
#   --skip-build       Skip cargo build (reuse existing binary)
#   --release          Build in release mode (default)
#   --debug            Build in debug mode
#   --bind <ADDR>      Bind address (default: 127.0.0.1:8080)
#   --rollup-port <N>  Rollup RPC port (default: 12346)
#   --defer            Defer sequencer submission (queue mode)
#   -h, --help         Show this help
#
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Defaults
SKIP_BUILD=0
RELEASE_MODE=1
BIND_ADDR="127.0.0.1:8080"
ROLLUP_PORT=12346
DEFER_FLAG=""

# Colors
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

# ----- Argument parsing -----------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build)   SKIP_BUILD=1 ;;
    --release)      RELEASE_MODE=1 ;;
    --debug)        RELEASE_MODE=0 ;;
    --bind)         BIND_ADDR="$2"; shift ;;
    --rollup-port)  ROLLUP_PORT="$2"; shift ;;
    --defer)        DEFER_FLAG="--defer-submission" ;;
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

VERIFIER_BIN="$REPO_ROOT/target/$TARGET_DIR/proof-verifier"

# Paths — must match what run_rollup.sh produces
ARTIFACTS_DIR="$SCRIPT_DIR/nightstream-artifacts"
ROLLUP_CONFIG="$ARTIFACTS_DIR/nightstream_rollup_config.toml"
SIGNING_KEY="$REPO_ROOT/examples/test-data/keys/token_deployer_private_key.json"
NODE_RPC_URL="http://127.0.0.1:$ROLLUP_PORT"
CHAIN_ID="${CHAIN_ID:-4321}"
LOG_LEVEL="${LOG_LEVEL:-info}"

# ----- Helpers --------------------------------------------------------------

step=0
print_step() { step=$((step+1)); echo ""; echo -e "${CYAN}${BOLD}[$step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

# ----- Preamble -------------------------------------------------------------

echo ""
echo -e "${BOLD}Nightstream Proof Verifier Service${NC}"
echo "═══════════════════════════════════"
echo ""
echo "  Repo root:     $REPO_ROOT"
echo "  Bind address:  $BIND_ADDR"
echo "  Rollup RPC:    $NODE_RPC_URL"
echo "  Proof backend: nightstream"
echo "  Rollup config: $ROLLUP_CONFIG"

# ----- Step 1: Build --------------------------------------------------------

if [ "$SKIP_BUILD" -eq 0 ]; then
  print_step "Building proof-verifier service"
  cargo build $CARGO_PROFILE -p sov-proof-verifier-service 2>&1 | tail -3
  print_ok "proof-verifier"
else
  print_step "Build skipped (--skip-build)"
  [ -f "$VERIFIER_BIN" ] || { echo "  Missing $VERIFIER_BIN"; exit 1; }
  print_ok "Binary present"
fi

# ----- Step 2: Validate config exists ---------------------------------------

print_step "Validating rollup config"

if [ ! -f "$ROLLUP_CONFIG" ]; then
  echo -e "  ${RED}ERROR${NC} Rollup config not found: $ROLLUP_CONFIG"
  echo "  Run 'examples/rollup-nightstream/run_rollup.sh' first to generate it."
  exit 1
fi

# Extract DA SQLite path from the resolved rollup config for display
DA_SQLITE=$(python3 -c "
import re, sys
config = open('$ROLLUP_CONFIG').read()
m = re.search(r'connection_string\s*=\s*\"([^\"]+)\"', config)
if m: print(m.group(1))
else: print('(could not parse)')
" 2>/dev/null || echo "(parse error)")

print_ok "Rollup config found"
print_info "DA connection: $DA_SQLITE"
echo ""
echo -e "  ${BOLD}The verifier shares this DB with the rollup for worker_verified_transactions.${NC}"

# ----- Step 3: Optional method ID args --------------------------------------

METHOD_ID_ARGS=()
if [ -n "${METHOD_ID:-}" ]; then
  METHOD_ID_ARGS+=(--method-id "$METHOD_ID")
fi
if [ -n "${MIDNIGHT_METHOD_ID:-}" ]; then
  METHOD_ID_ARGS+=(--midnight-method-id "$MIDNIGHT_METHOD_ID")
fi

MAX_CONCURRENT_ARGS=()
if [ -n "${MAX_CONCURRENT:-}" ]; then
  MAX_CONCURRENT_ARGS+=(--max-concurrent "$MAX_CONCURRENT")
fi

# ----- Step 4: Start the verifier -------------------------------------------

print_step "Starting proof-verifier-service"

echo ""
echo -e "  ${BOLD}Endpoints:${NC}"
echo "    POST http://$BIND_ADDR/midnight-privacy"
echo "    POST http://$BIND_ADDR/prove"
echo "    POST http://$BIND_ADDR/verify"
echo "    GET  http://$BIND_ADDR/health"
echo ""
echo "────────────────────────────────────────────────────"
echo ""

exec "$VERIFIER_BIN" \
  "${METHOD_ID_ARGS[@]}" \
  "${MAX_CONCURRENT_ARGS[@]}" \
  --bind "$BIND_ADDR" \
  --node-rpc-url "$NODE_RPC_URL" \
  --signing-key-path "$SIGNING_KEY" \
  --chain-id "$CHAIN_ID" \
  --log-level "$LOG_LEVEL" \
  --rollup-config-path "$ROLLUP_CONFIG" \
  --proof-backend nightstream \
  $DEFER_FLAG
