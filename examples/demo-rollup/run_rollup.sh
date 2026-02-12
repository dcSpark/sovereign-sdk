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
#   examples/demo-rollup/run_rollup.sh [OPTIONS]
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
ARTIFACTS_DIR="$REPO_ROOT/nightstream-artifacts"
GENESIS_TEMPLATE_DIR="$REPO_ROOT/examples/test-data/genesis/demo/mock"

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

# Rollup data lives next to mock_rollup_config.toml (relative path "demo_data")
DATA_DIR="$REPO_ROOT/demo_data"
DA_SQLITE="$REPO_ROOT/mock_da.sqlite"

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

# ----- Step 5: Write rollup config -------------------------------------------

print_step "Writing Nightstream rollup config"

# Nightstream verify-only takes ~100ms in release, ~2-5s in debug.
# Set batch_execution_time_limit generously for circuit synthesis overhead.
if [ "$RELEASE_MODE" -eq 1 ]; then
  BATCH_LIMIT=5000
else
  BATCH_LIMIT=120000
fi

ROLLUP_CONFIG="$ARTIFACTS_DIR/nightstream_rollup_config.toml"
cat > "$ROLLUP_CONFIG" << TOML
# Auto-generated Nightstream-compatible rollup config
# Based on examples/demo-rollup/mock_rollup_config.toml with adjusted limits.

[da]
connection_string = "sqlite://$DA_SQLITE?mode=rwc"
sender_address = "0000000000000000000000000000000000000000000000000000000000000000"
finalization = 40
[da.block_producing.periodic]
block_time_ms = 2000

[storage]
path = "$DATA_DIR"
state_cache_size = 4294967296

[runner]
genesis_height = 0
da_polling_interval_ms = 50

[runner.http_config]
bind_host = "127.0.0.1"
bind_port = $ROLLUP_PORT

[monitoring]
telegraf_address = "udp://127.0.0.1:8094"

[proof_manager]
aggregated_proof_block_jump = 16
prover_address = "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"
max_number_of_transitions_in_db = 100
max_number_of_transitions_in_memory = 30

[sequencer]
blob_processing_timeout_secs = 3000
max_batch_size_bytes = 20971520
max_concurrent_blobs = 512
max_allowed_node_distance_behind = 10
rollup_address = "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"
[sequencer.preferred]
disable_state_root_consistency_checks = true
recovery_strategy = "TryToSave"
batch_execution_time_limit_millis = $BATCH_LIMIT
num_cache_warmup_workers = 5
[sequencer.extension]
max_log_limit = 20000
TOML

print_ok "Config: $ROLLUP_CONFIG (batch_limit=${BATCH_LIMIT}ms)"

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
