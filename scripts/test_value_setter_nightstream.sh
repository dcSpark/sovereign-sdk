#!/usr/bin/env bash
# ============================================================================
# test_value_setter_nightstream.sh
#
# End-to-end test for the Value Setter ZK module with the Nightstream backend.
#
# This script exercises the full pipeline:
#   1. (Optional) Rebuild the value-validator RISC-V guest if --rebuild-guest
#   2. Run Nightstream standalone prove+verify tests (neo-fold)
#   3. Run the sov-nightstream-adapter integration tests
#   4. Run the sov-value-setter-zk tests with the nightstream feature
#
# Usage:
#   scripts/test_value_setter_nightstream.sh [OPTIONS]
#
# Options:
#   --rebuild-guest    Rebuild the value-validator guest and re-extract ROM
#   --release          Run tests in release mode (faster proving, slower compile)
#   --verbose          Show full cargo output (no tail)
#   --adapter-only     Only run the adapter tests, skip Nightstream standalone
#   -h, --help         Show this help
#
# Requirements:
#   - Nightstream repo at ../Nightstream (relative to sovereign-sdk root)
#   - Rust toolchain with riscv32im-unknown-none-elf target (for --rebuild-guest)
# ============================================================================
set -euo pipefail

# ----- Configuration --------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
NIGHTSTREAM_ROOT="$(cd "$REPO_ROOT/../Nightstream" 2>/dev/null && pwd || echo "")"

REBUILD_GUEST=0
RELEASE_MODE=0
VERBOSE=0
ADAPTER_ONLY=0

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# ----- Argument parsing -----------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --rebuild-guest) REBUILD_GUEST=1 ;;
    --release)       RELEASE_MODE=1 ;;
    --verbose)       VERBOSE=1 ;;
    --adapter-only)  ADAPTER_ONLY=1 ;;
    -h|--help)
      head -28 "$0" | tail -22
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
  shift
done

CARGO_PROFILE=""
if [ "$RELEASE_MODE" -eq 1 ]; then
  CARGO_PROFILE="--release"
fi

# ----- Helpers --------------------------------------------------------------

step=0
pass=0
fail=0

print_header() {
  step=$((step + 1))
  echo ""
  echo -e "${CYAN}${BOLD}[$step] $1${NC}"
  echo "────────────────────────────────────────────────────"
}

print_ok() {
  pass=$((pass + 1))
  echo -e "    ${GREEN}OK${NC} $1"
}

print_fail() {
  fail=$((fail + 1))
  echo -e "    ${RED}FAIL${NC} $1"
}

print_skip() {
  echo -e "    ${YELLOW}SKIP${NC} $1"
}

run_cmd() {
  local desc="$1"
  shift
  if [ "$VERBOSE" -eq 1 ]; then
    if "$@"; then
      print_ok "$desc"
    else
      print_fail "$desc"
      return 1
    fi
  else
    local tmpfile
    tmpfile="$(mktemp)"
    if "$@" > "$tmpfile" 2>&1; then
      print_ok "$desc"
      rm -f "$tmpfile"
    else
      print_fail "$desc"
      echo -e "    ${RED}--- Last 30 lines of output ---${NC}"
      tail -30 "$tmpfile" | sed 's/^/    /'
      rm -f "$tmpfile"
      return 1
    fi
  fi
}

# ----- Preamble -------------------------------------------------------------

echo ""
echo -e "${BOLD}Nightstream Value Setter ZK -- End-to-End Test${NC}"
echo "================================================"
echo ""
echo "  Sovereign SDK repo : $REPO_ROOT"

if [ -z "$NIGHTSTREAM_ROOT" ]; then
  echo -e "  Nightstream repo   : ${RED}NOT FOUND${NC} (expected at ../Nightstream)"
  if [ "$ADAPTER_ONLY" -eq 0 ]; then
    echo ""
    echo "Cannot run Nightstream standalone tests without the Nightstream repo."
    echo "Use --adapter-only to skip them, or ensure ../Nightstream exists."
    exit 1
  fi
else
  echo "  Nightstream repo   : $NIGHTSTREAM_ROOT"
fi

echo "  Release mode       : $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"
echo "  Rebuild guest      : $([ "$REBUILD_GUEST" -eq 1 ] && echo yes || echo no)"
echo "  Adapter only       : $([ "$ADAPTER_ONLY" -eq 1 ] && echo yes || echo no)"

# ----- Step 1: Optionally rebuild the guest ---------------------------------

if [ "$REBUILD_GUEST" -eq 1 ]; then
  print_header "Rebuilding value-validator RISC-V guest"

  if [ -z "$NIGHTSTREAM_ROOT" ]; then
    print_fail "Nightstream repo not found, cannot rebuild guest"
    exit 1
  fi

  GUEST_DIR="$NIGHTSTREAM_ROOT/crates/neo-fold/riscv-tests/guests/value-validator"

  # Ensure the riscv target is installed
  if ! rustup target list --installed --toolchain 1.88 2>/dev/null | grep -q riscv32im; then
    echo "  Installing riscv32im-unknown-none-elf target..."
    rustup target add riscv32im-unknown-none-elf --toolchain 1.88
  fi

  run_cmd "cargo build --release (guest)" \
    cargo build --release --manifest-path "$GUEST_DIR/Cargo.toml"

  run_cmd "export_rom_rs.py (extract .neo_start ROM)" \
    python3 "$GUEST_DIR/export_rom_rs.py"

  ROM_FILE="$NIGHTSTREAM_ROOT/crates/neo-fold/riscv-tests/binaries/value_validator_rom.rs"
  if [ -f "$ROM_FILE" ]; then
    ROM_SIZE=$(grep -o 'u8; [0-9]*' "$ROM_FILE" | head -1 | awk '{print $2}')
    ROM_BASE=$(grep 'ROM_BASE' "$ROM_FILE" | grep -o '[0-9]*u64' | head -1 | sed 's/u64//')
    echo "  ROM: ${ROM_SIZE:-?} bytes, base=${ROM_BASE:-?}"
  fi
else
  print_header "Guest rebuild skipped (use --rebuild-guest to enable)"
  print_skip "Using pre-built ROM from Nightstream repo"
fi

# ----- Step 2: Nightstream standalone prove+verify --------------------------

if [ "$ADAPTER_ONLY" -eq 0 ]; then
  print_header "Nightstream standalone prove+verify (neo-fold)"

  run_cmd "test_value_validator_prove_verify (value=42)" \
    cargo test $CARGO_PROFILE \
      --manifest-path "$NIGHTSTREAM_ROOT/Cargo.toml" \
      -p neo-fold \
      --test test_value_validator_compiled_full_prove_verify \
      -- test_value_validator_prove_verify --nocapture

  run_cmd "test_value_validator_max_range (value=65535)" \
    cargo test $CARGO_PROFILE \
      --manifest-path "$NIGHTSTREAM_ROOT/Cargo.toml" \
      -p neo-fold \
      --test test_value_validator_compiled_full_prove_verify \
      -- test_value_validator_max_range --nocapture

  run_cmd "test_value_validator_zero (value=0)" \
    cargo test $CARGO_PROFILE \
      --manifest-path "$NIGHTSTREAM_ROOT/Cargo.toml" \
      -p neo-fold \
      --test test_value_validator_compiled_full_prove_verify \
      -- test_value_validator_zero --nocapture
else
  print_header "Nightstream standalone tests skipped (--adapter-only)"
fi

# ----- Step 3: sov-nightstream-adapter tests --------------------------------

print_header "Sovereign SDK adapter tests (sov-nightstream-adapter)"

run_cmd "Unit tests (commitment codec, crypto, hash)" \
  cargo test $CARGO_PROFILE \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    -p sov-nightstream-adapter --features native \
    --lib -- --nocapture

run_cmd "Integration: code_commitment_roundtrip" \
  cargo test $CARGO_PROFILE \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    -p sov-nightstream-adapter --features native \
    --test integration_test \
    -- test_code_commitment_roundtrip --nocapture

run_cmd "Integration: prove_and_verify_value_validator (full round-trip)" \
  cargo test $CARGO_PROFILE \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    -p sov-nightstream-adapter --features native \
    --test integration_test \
    -- test_prove_and_verify_value_validator --nocapture

run_cmd "Integration: verify_with_wrong_commitment (negative test)" \
  cargo test $CARGO_PROFILE \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    -p sov-nightstream-adapter --features native \
    --test integration_test \
    -- test_verify_with_wrong_commitment --nocapture

# ----- Step 4: sov-value-setter-zk with nightstream feature ----------------

print_header "Value Setter ZK module (sov-value-setter-zk + nightstream)"

run_cmd "Unit + integration tests (with nightstream feature)" \
  cargo test $CARGO_PROFILE \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    -p sov-value-setter-zk --features "native,nightstream" \
    --lib --tests -- --nocapture

run_cmd "Compile check: value-setter-zk with nightstream (no native)" \
  cargo check \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    -p sov-value-setter-zk --features nightstream

# ----- Summary --------------------------------------------------------------

echo ""
echo "════════════════════════════════════════════════════"
total=$((pass + fail))
if [ "$fail" -eq 0 ]; then
  echo -e "${GREEN}${BOLD}  ALL $total CHECKS PASSED${NC}"
else
  echo -e "${RED}${BOLD}  $fail / $total CHECKS FAILED${NC}"
fi
echo "════════════════════════════════════════════════════"
echo ""

exit "$fail"
