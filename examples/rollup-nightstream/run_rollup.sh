#!/usr/bin/env bash
# ============================================================================
# run_rollup.sh  --  Start a Nightstream-compatible mock rollup
#
# This script handles:
#   - Building the rollup binary (with --features nightstream)
#   - Generating genesis config (code commitment only -- no proof generation)
#   - Cleaning stale state from previous runs (unless --keep-state)
#   - Starting the rollup with Nightstream-friendly settings
#
# Usage:
#   examples/rollup-nightstream/run_rollup.sh [OPTIONS]
#
# Options:
#   --skip-build       Skip cargo build (reuse existing binaries)
#   --keep-state       Don't wipe demo_data/ on startup (resume previous chain)
#   --release          Build and run in release mode (default)
#   --debug            Build and run in debug mode
#   --port <N>         HTTP port (default: 12346)
#   --skip-verify      Set NIGHTSTREAM_SKIP_VERIFICATION=1 (skip proof verification)
#   -h, --help         Show this help
#
# To generate proofs and submit transactions, use the dedicated script:
#
#   scripts/test_nightstream_e2e_rollup.sh --no-rollup [--skip-build]
#
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACTS_DIR="$SCRIPT_DIR/nightstream-artifacts"
GENESIS_TEMPLATE_DIR="$REPO_ROOT/examples/test-data/genesis/demo/mock"
ROLLUP_CONFIG_TEMPLATE="$SCRIPT_DIR/rollup_config.toml"

# Defaults
SKIP_BUILD=0
KEEP_STATE=0
RELEASE_MODE=1
ROLLUP_PORT=12346
SKIP_VERIFY=0

# Colors
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

# ----- Argument parsing -----------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build)  SKIP_BUILD=1 ;;
    --keep-state)  KEEP_STATE=1 ;;
    --release)     RELEASE_MODE=1 ;;
    --debug)       RELEASE_MODE=0 ;;
    --port)        ROLLUP_PORT="$2"; shift ;;
    --skip-verify) SKIP_VERIFY=1 ;;
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

SOV_ROLLUP="$REPO_ROOT/target/$TARGET_DIR/sov-demo-rollup"
PROOF_GEN="$REPO_ROOT/target/$TARGET_DIR/examples/generate_proof_tx"
SOV_CLI="$REPO_ROOT/target/$TARGET_DIR/sov-cli"

# Rollup data lives relative to where we run
DATA_DIR="$SCRIPT_DIR/demo_data"
DA_SQLITE="$SCRIPT_DIR/mock_da.sqlite"

# ----- Helpers --------------------------------------------------------------

step=0
print_step() { step=$((step+1)); echo ""; echo -e "${CYAN}${BOLD}[$step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

# ----- Preamble -------------------------------------------------------------

echo ""
echo -e "${BOLD}Nightstream Mock Rollup${NC}"
echo "═══════════════════════"
echo ""
echo "  Repo root:     $REPO_ROOT"
echo "  Script dir:    $SCRIPT_DIR"
echo "  Release mode:  $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"
echo "  Port:          $ROLLUP_PORT"
echo "  Skip verify:   $([ "$SKIP_VERIFY" -eq 1 ] && echo yes || echo no)"
echo "  Keep state:    $([ "$KEEP_STATE" -eq 1 ] && echo yes || echo no)"

# ----- Step 1: Build --------------------------------------------------------

if [ "$SKIP_BUILD" -eq 0 ]; then
  print_step "Building binaries"

  print_info "Building genesis generator..."
  cargo build $CARGO_PROFILE \
    -p sov-nightstream-adapter --example generate_proof_tx --features native \
    2>&1 | tail -3
  print_ok "generate_proof_tx"

  print_info "Building rollup + CLI (SKIP_GUEST_BUILD=1)..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE \
    --bin sov-demo-rollup --bin sov-cli \
    -p sov-demo-rollup --features nightstream \
    2>&1 | tail -3
  print_ok "sov-demo-rollup + sov-cli"
else
  print_step "Build skipped (--skip-build)"
  [ -f "$SOV_ROLLUP" ] || { echo "  Missing $SOV_ROLLUP"; exit 1; }
  [ -f "$PROOF_GEN" ]  || { echo "  Missing $PROOF_GEN"; exit 1; }
  print_ok "Binaries present"
fi

# ----- Step 2: Generate genesis config (fast, no proof) ---------------------

print_step "Generating genesis config (--genesis-only, no proof)"

mkdir -p "$ARTIFACTS_DIR"
"$PROOF_GEN" --genesis-only --output-dir "$ARTIFACTS_DIR" 2>&1 | sed 's/^/  /'

GENESIS_VSZ="$ARTIFACTS_DIR/value_setter_zk.json"
[ -f "$GENESIS_VSZ" ] || { echo "  Genesis generation failed"; exit 1; }
print_ok "Genesis artifacts in $ARTIFACTS_DIR"

# ----- Step 3: Prepare genesis -----------------------------------------------

print_step "Preparing genesis directory"

GENESIS_DIR="$ARTIFACTS_DIR/genesis"
mkdir -p "$GENESIS_DIR"
cp "$GENESIS_TEMPLATE_DIR"/*.json "$GENESIS_DIR/"
cp "$GENESIS_VSZ" "$GENESIS_DIR/value_setter_zk.json"
print_ok "Genesis ready at $GENESIS_DIR"

# ----- Step 4: Clean stale state --------------------------------------------

if [ "$KEEP_STATE" -eq 0 ]; then
  print_step "Cleaning previous rollup state"
  rm -rf "$DATA_DIR" "$DA_SQLITE" "${DA_SQLITE}-wal" "${DA_SQLITE}-shm"
  print_ok "Cleaned demo_data/ and mock_da.sqlite"
else
  print_step "Keeping previous state (--keep-state)"
  print_info "Data dir: $DATA_DIR"
fi

# ----- Step 5: Write rollup config with resolved paths -----------------------

print_step "Resolving rollup config paths"

# The template config uses relative paths; resolve them for this instance.
ROLLUP_CONFIG="$ARTIFACTS_DIR/nightstream_rollup_config.toml"
sed \
  -e "s|mock_da.sqlite|$DA_SQLITE|g" \
  -e "s|demo_data|$DATA_DIR|g" \
  -e "s|bind_port = 12346|bind_port = $ROLLUP_PORT|g" \
  "$ROLLUP_CONFIG_TEMPLATE" > "$ROLLUP_CONFIG"

# Adjust batch_execution_time_limit for debug mode
if [ "$RELEASE_MODE" -eq 0 ]; then
  sed -i.bak "s|batch_execution_time_limit_millis = 5000|batch_execution_time_limit_millis = 120000|g" "$ROLLUP_CONFIG"
  rm -f "$ROLLUP_CONFIG.bak"
fi

print_ok "Config: $ROLLUP_CONFIG"

# ----- Step 6: Start the rollup ---------------------------------------------

print_step "Starting rollup"

echo ""
echo -e "  ${BOLD}To generate a proof and submit a tx, run in another terminal:${NC}"
echo ""
echo "    scripts/test_nightstream_e2e_rollup.sh --no-rollup --skip-build"
echo ""
echo -e "  ${BOLD}Or manually with sov-cli:${NC}"
echo ""
echo "    $SOV_CLI node set-url http://127.0.0.1:$ROLLUP_PORT"
echo "    $SOV_CLI keys import --skip-if-present --nickname mykey \\"
echo "      --path examples/test-data/keys/token_deployer_private_key.json"
echo "    $SOV_CLI transactions import from-file value-setter-zk \\"
echo "      --chain-id 4321 --max-fee 10000000000 \\"
echo "      --path <PATH_TO_TX_JSON>"
echo "    NONCE=\$(python3 -c 'import time; print(int(time.time()*1000))')"
echo "    $SOV_CLI node submit-batch \$NONCE by-nickname mykey"
echo ""
echo -e "  ${BOLD}Check tx result:${NC}"
echo "    curl 'http://127.0.0.1:$ROLLUP_PORT/ledger/txs/<TX_HASH>?children=1' | python3 -m json.tool"
echo ""
echo "────────────────────────────────────────────────────"
echo ""

EXTRA_ENV=""
if [ "$SKIP_VERIFY" -eq 1 ]; then
  EXTRA_ENV="NIGHTSTREAM_SKIP_VERIFICATION=1"
  print_info "NIGHTSTREAM_SKIP_VERIFICATION=1 (proof verification skipped)"
fi

exec env $EXTRA_ENV \
  RISC0_DEV_MODE=true \
  RUST_LOG="info,sov_value_setter_zk=debug,sov_nightstream_adapter=debug" \
  "$SOV_ROLLUP" \
  --da-layer mock \
  --rollup-config-path "$ROLLUP_CONFIG" \
  --genesis-config-dir "$GENESIS_DIR"
