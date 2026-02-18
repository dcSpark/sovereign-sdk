#!/usr/bin/env bash
# ============================================================================
# run_rollup.sh  --  Start a Nightstream-compatible rollup (midnight DA)
#
# Uses demo-rollup with Nightstream proving and midnight DA
# (SQLite). The proof-verifier-service can share the same DA SQLite to
# exchange worker_verified_transactions.
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
#   --skip-verify      Set NIGHTSTREAM_SKIP_VERIFICATION=1
#   -h, --help         Show this help
#
# To run the proof-verifier-service alongside:
#
#   examples/rollup-nightstream/run_verifier.sh [--skip-build]
#
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
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

SOV_ROLLUP="$REPO_ROOT/target/$TARGET_DIR/sov-rollup-nightstream"
PROOF_GEN="$REPO_ROOT/target/$TARGET_DIR/examples/generate_proof_tx"

# Runtime directories (relative to where the rollup runs from)
ARTIFACTS_DIR="$SCRIPT_DIR/nightstream-artifacts"
DATA_DIR="$SCRIPT_DIR/demo_data"
DA_SQLITE="$SCRIPT_DIR/da.sqlite"

# ----- Helpers --------------------------------------------------------------

step=0
print_step() { step=$((step+1)); echo ""; echo -e "${CYAN}${BOLD}[$step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

# ----- Preamble -------------------------------------------------------------

echo ""
echo -e "${BOLD}Nightstream Rollup (midnight DA + SQLite)${NC}"
echo "═══════════════════════════════════════════"
echo ""
echo "  Repo root:     $REPO_ROOT"
echo "  Script dir:    $SCRIPT_DIR"
echo "  Release mode:  $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"
echo "  Port:          $ROLLUP_PORT"
echo "  Skip verify:   $([ "$SKIP_VERIFY" -eq 1 ] && echo yes || echo no)"
echo "  Keep state:    $([ "$KEEP_STATE" -eq 1 ] && echo yes || echo no)"
echo "  DA SQLite:     $DA_SQLITE"

# ----- Step 1: Build --------------------------------------------------------

if [ "$SKIP_BUILD" -eq 0 ]; then
  print_step "Building binaries"

  print_info "Building genesis generator (generate_proof_tx)..."
  cargo build $CARGO_PROFILE \
    -p sov-nightstream-adapter --example generate_proof_tx --features native \
    2>&1 | tail -3
  print_ok "generate_proof_tx"

  print_info "Building rollup-nightstream..."
  SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE \
    -p sov-rollup-nightstream \
    2>&1 | tail -3
  print_ok "rollup-nightstream"
else
  print_step "Build skipped (--skip-build)"
  [ -f "$SOV_ROLLUP" ] || { echo "  Missing $SOV_ROLLUP"; exit 1; }
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

# Compute Nightstream method_id (SHA-256 of NOTE_SPEND_ROM) and inject into
# midnight_privacy.json with backend=nightstream.
print_info "Computing Nightstream midnight method_id from note_spend_rom.rs..."
NS_METHOD_ID_HEX=$(python3 -c "
import hashlib, re, pathlib
rom_rs = pathlib.Path('$REPO_ROOT/crates/adapters/nightstream/circuits/note_spend_rom.rs').read_text()
# Extract byte literals from the ROM array
rom_bytes = bytes(int(b, 16) for b in re.findall(r'0x([0-9a-fA-F]{2})', rom_rs))
print(hashlib.sha256(rom_bytes).hexdigest())
")
NS_METHOD_ID_JSON=$(python3 -c "
h = '$NS_METHOD_ID_HEX'
print('[' + ', '.join(str(int(h[i:i+2], 16)) for i in range(0, len(h), 2)) + ']')
")
print_ok "Nightstream method_id: 0x${NS_METHOD_ID_HEX}"

# Patch midnight_privacy.json: set method_id
python3 -c "
import json, sys
mp = json.load(open('$GENESIS_DIR/midnight_privacy.json'))
mp['method_id'] = $NS_METHOD_ID_JSON
json.dump(mp, open('$GENESIS_DIR/midnight_privacy.json', 'w'), indent=2)
print('  Patched midnight_privacy.json: method_id set')
"

print_ok "Genesis ready at $GENESIS_DIR"

# ----- Step 4: Clean stale state --------------------------------------------

if [ "$KEEP_STATE" -eq 0 ]; then
  print_step "Cleaning previous rollup state"
  rm -rf "$DATA_DIR" "$DA_SQLITE" "${DA_SQLITE}-wal" "${DA_SQLITE}-shm"
  print_ok "Cleaned demo_data/ and da.sqlite"

  if command -v psql >/dev/null 2>&1; then
    PG_BASE_URL=$(grep -m1 'connection_string' "$ROLLUP_CONFIG_TEMPLATE" \
      | sed 's/.*"\(.*\)"/\1/' | sed 's|/[^/]*$||')
    PG_DATABASES=("da" "indexer" "fvk" "mcp_sessions")
    for db in "${PG_DATABASES[@]}"; do
      db_url="${PG_BASE_URL}/${db}"
      tables=$(psql "$db_url" -v ON_ERROR_STOP=1 -Atqc \
        "SELECT string_agg(quote_ident(schemaname) || '.' || quote_ident(tablename), ', ') \
         FROM pg_tables WHERE schemaname = 'public';" 2>/dev/null) || continue
      if [ -n "$tables" ]; then
        psql "$db_url" -v ON_ERROR_STOP=1 -q -c \
          "TRUNCATE TABLE $tables RESTART IDENTITY CASCADE;" >/dev/null 2>&1 \
          && print_ok "Truncated Postgres tables in '$db'" \
          || print_info "Could not truncate '$db' (may not exist yet)"
      fi
    done
  else
    print_info "psql not found — skipping Postgres cleanup"
  fi
else
  print_step "Keeping previous state (--keep-state)"
  print_info "Data dir: $DATA_DIR"
fi

# ----- Step 5: Write rollup config with resolved paths -----------------------

print_step "Resolving rollup config paths"

ROLLUP_CONFIG="$ARTIFACTS_DIR/nightstream_rollup_config.toml"
sed \
  -e "s|da.sqlite|$DA_SQLITE|g" \
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
echo -e "  ${BOLD}To start the proof-verifier-service, run in another terminal:${NC}"
echo ""
echo "    examples/rollup-nightstream/run_verifier.sh --skip-build"
echo ""
echo -e "  ${BOLD}DA SQLite (shared with verifier):${NC}"
echo "    $DA_SQLITE"
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

# Create data directory (required for SQLite)
mkdir -p "$DATA_DIR"

exec env $EXTRA_ENV \
  RISC0_DEV_MODE=true \
  RUST_LOG="${RUST_LOG:-info,midnight_privacy=debug,sov_nightstream_adapter=debug}" \
  "$SOV_ROLLUP" \
  --rollup-config-path "$ROLLUP_CONFIG" \
  --genesis-config-dir "$GENESIS_DIR"
