#!/usr/bin/env bash
# ============================================================================
# test_nightstream_e2e_rollup.sh
#
# End-to-end test: generate a Nightstream proof, build a transaction with
# sov-cli, submit it to a running mock rollup, and verify the value was set.
#
# Usage:
#   scripts/test_nightstream_e2e_rollup.sh [OPTIONS]
#
# Options:
#   --value <N>        Value to prove and set (default: 42, range [0, 65535])
#   --skip-build       Skip cargo build step (reuse existing binaries)
#   --keep-rollup      Don't stop the rollup after the test
#   --release          Build in release mode
#   --full-verify      Enable on-chain proof verification (use with --release
#                      for a realistic test)
#   --generate-only    Only generate proof + tx payload + genesis config, then
#                      stop.  Prints paths and manual instructions.
#   --output-dir <D>   Write artifacts to <D> instead of a temp directory.
#                      Implies --keep-rollup (directory is never deleted).
#   --no-rollup [URL]  Don't start a rollup; connect to an already-running one.
#                      URL defaults to http://127.0.0.1:12346.
#                      Generates a fresh proof, submits it, polls the receipt.
#                      Pair with: examples/demo-rollup/run_rollup.sh
#   -h, --help         Show this help
#
# Requirements:
#   - Nightstream repo at ../Nightstream (relative to sovereign-sdk root)
# ============================================================================
set -euo pipefail

# ----- Configuration --------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
GENESIS_TEMPLATE_DIR="$REPO_ROOT/examples/test-data/genesis/demo/mock"
KEY_PATH="$REPO_ROOT/examples/test-data/keys/token_deployer_private_key.json"
KEY_NICKNAME="NIGHTSTREAM_TEST_KEY"
CHAIN_ID=4321
ROLLUP_PORT=12346
ROLLUP_URL="http://127.0.0.1:$ROLLUP_PORT"

TEST_VALUE=42
SKIP_BUILD=0
KEEP_ROLLUP=0
RELEASE_MODE=0
FULL_VERIFY=0
GENERATE_ONLY=0
USER_OUTPUT_DIR=""
NO_ROLLUP=0
EXTERNAL_ROLLUP_URL=""

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# ----- Argument parsing -----------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --value)       TEST_VALUE="$2"; shift ;;
    --skip-build)  SKIP_BUILD=1 ;;
    --keep-rollup) KEEP_ROLLUP=1 ;;
    --release)     RELEASE_MODE=1 ;;
    --full-verify)    FULL_VERIFY=1 ;;
    --generate-only)  GENERATE_ONLY=1 ;;
    --output-dir)     USER_OUTPUT_DIR="$2"; shift ;;
    --no-rollup)
      NO_ROLLUP=1
      # Optional next arg is the rollup URL (if it doesn't start with --)
      if [ $# -ge 2 ] && [[ "$2" != --* ]]; then
        EXTERNAL_ROLLUP_URL="$2"; shift
      fi
      ;;
    -h|--help)
      sed -n '2,/^# =====/{/^# =====/d;s/^# \?//;p}' "$0"
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
CARGO_TARGET_DIR="debug"
if [ "$RELEASE_MODE" -eq 1 ]; then
  CARGO_PROFILE="--release"
  CARGO_TARGET_DIR="release"
fi

SOV_CLI="$REPO_ROOT/target/$CARGO_TARGET_DIR/sov-cli"
SOV_ROLLUP="$REPO_ROOT/target/$CARGO_TARGET_DIR/sov-demo-rollup"
PROOF_GEN="$REPO_ROOT/target/$CARGO_TARGET_DIR/examples/generate_proof_tx"

# --no-rollup: override the rollup URL if provided
if [ "$NO_ROLLUP" -eq 1 ]; then
  if [ -n "$EXTERNAL_ROLLUP_URL" ]; then
    ROLLUP_URL="$EXTERNAL_ROLLUP_URL"
  fi
  # Extract port from URL for display
  ROLLUP_PORT=$(echo "$ROLLUP_URL" | grep -oE ':[0-9]+$' | tr -d ':' || echo "12346")
fi

# Workspace for artifacts and runtime data
if [ -n "$USER_OUTPUT_DIR" ]; then
  WORK_DIR="$(cd "$REPO_ROOT" && mkdir -p "$USER_OUTPUT_DIR" && cd "$USER_OUTPUT_DIR" && pwd)"
  KEEP_ROLLUP=1   # never delete a user-chosen directory
else
  WORK_DIR="$(mktemp -d)"
fi
GENESIS_DIR="$WORK_DIR/genesis"
ROLLUP_DATA_DIR="$WORK_DIR/rollup_data"
ROLLUP_PID=""
FINAL_RESULT=""   # will be set to PASS or FAIL

# ----- Helpers --------------------------------------------------------------

step=0
print_header() { step=$((step+1)); echo ""; echo -e "${CYAN}${BOLD}[$step] $1${NC}"; echo "────────────────────────────────────────────────────"; }
print_ok()   { echo -e "  ${GREEN}OK${NC} $1"; }
print_fail() { echo -e "  ${RED}FAIL${NC} $1"; }
print_info() { echo -e "  ${YELLOW}INFO${NC} $1"; }

cleanup() {
  local exit_code=$?
  if [ -n "$ROLLUP_PID" ] && kill -0 "$ROLLUP_PID" 2>/dev/null; then
    if [ "$KEEP_ROLLUP" -eq 0 ]; then
      echo ""; echo -e "${YELLOW}Stopping rollup (PID $ROLLUP_PID)...${NC}"
      kill "$ROLLUP_PID" 2>/dev/null || true
      wait "$ROLLUP_PID" 2>/dev/null || true
    else
      echo ""; echo -e "${YELLOW}Rollup still running (PID $ROLLUP_PID) -- use: kill $ROLLUP_PID${NC}"
    fi
  fi
  "$SOV_CLI" transactions clean 2>/dev/null || true
  if [ "$KEEP_ROLLUP" -eq 0 ]; then rm -rf "$WORK_DIR"; else echo -e "${YELLOW}Work dir preserved: $WORK_DIR${NC}"; fi
  exit $exit_code
}
trap cleanup EXIT INT TERM

wait_for_rollup() {
  local url="$1" timeout="${2:-60}" elapsed=0
  while [ $elapsed -lt $timeout ]; do
    curl -sf "$url/healthcheck" > /dev/null 2>&1 && return 0
    sleep 1; elapsed=$((elapsed+1))
  done
  return 1
}

# ----- Preamble -------------------------------------------------------------

echo ""
if [ "$GENERATE_ONLY" -eq 1 ]; then
  echo -e "${BOLD}Nightstream Proof + TX Generator${NC}"
elif [ "$NO_ROLLUP" -eq 1 ]; then
  echo -e "${BOLD}Nightstream E2E Test (external rollup)${NC}"
else
  echo -e "${BOLD}Nightstream E2E Rollup Test${NC}"
fi
echo "═══════════════════════════════════"
echo ""
echo "  Value:         $TEST_VALUE"
if [ "$NO_ROLLUP" -eq 1 ]; then
  echo "  Mode:          no-rollup (external)"
  echo "  Rollup URL:    $ROLLUP_URL"
elif [ "$GENERATE_ONLY" -eq 1 ]; then
  echo "  Mode:          generate-only"
else
  echo "  Mode:          full e2e"
fi
echo "  Full verify:   $([ "$FULL_VERIFY" -eq 1 ] && echo yes || echo "no (skip verification)")"
echo "  Release mode:  $([ "$RELEASE_MODE" -eq 1 ] && echo yes || echo no)"
echo "  Output dir:    $WORK_DIR"

# ----- Step 1: Build binaries -----------------------------------------------

if [ "$SKIP_BUILD" -eq 0 ]; then
  print_header "Building binaries"
  print_info "Building proof generator..."
  cargo build $CARGO_PROFILE -p sov-nightstream-adapter --example generate_proof_tx --features native 2>&1 | tail -3
  print_ok "generate_proof_tx"

  if [ "$GENERATE_ONLY" -eq 0 ] && [ "$NO_ROLLUP" -eq 0 ]; then
    print_info "Building rollup + CLI (SKIP_GUEST_BUILD=1)..."
    SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE --bin sov-demo-rollup --bin sov-cli -p sov-demo-rollup --features nightstream 2>&1 | tail -3
    print_ok "sov-demo-rollup + sov-cli"
  elif [ "$NO_ROLLUP" -eq 1 ]; then
    print_info "Building sov-cli only (--no-rollup)..."
    SKIP_GUEST_BUILD=1 cargo build $CARGO_PROFILE --bin sov-cli -p sov-demo-rollup --features nightstream 2>&1 | tail -3
    print_ok "sov-cli"
  fi
else
  print_header "Build skipped (--skip-build)"
  [ -f "$PROOF_GEN" ] || { print_fail "Missing $PROOF_GEN"; exit 1; }
  if [ "$GENERATE_ONLY" -eq 0 ] && [ "$NO_ROLLUP" -eq 0 ]; then
    for bin in "$SOV_ROLLUP" "$SOV_CLI"; do
      [ -f "$bin" ] || { print_fail "Missing $bin"; exit 1; }
    done
  elif [ "$NO_ROLLUP" -eq 1 ]; then
    [ -f "$SOV_CLI" ] || { print_fail "Missing $SOV_CLI"; exit 1; }
  fi
  print_ok "Required binaries present"
fi

# ----- Step 2: Generate proof + transaction JSON ----------------------------

print_header "Generating Nightstream proof (value=$TEST_VALUE)"

"$PROOF_GEN" --value "$TEST_VALUE" --output-dir "$WORK_DIR" 2>&1 | sed 's/^/  /'

TX_JSON="$WORK_DIR/nightstream_set_value_tx.json"
GENESIS_VALUE_SETTER_ZK="$WORK_DIR/value_setter_zk.json"
[ -f "$TX_JSON" ] && [ -f "$GENESIS_VALUE_SETTER_ZK" ] || { print_fail "Proof generator output missing"; exit 1; }
print_ok "Proof and transaction JSON generated"

# ----- Step 3: Prepare genesis config (skipped in --no-rollup mode) ---------

if [ "$NO_ROLLUP" -eq 0 ]; then
  print_header "Preparing genesis config (nightstream backend)"

  mkdir -p "$GENESIS_DIR"
  cp "$GENESIS_TEMPLATE_DIR"/*.json "$GENESIS_DIR/"
  cp "$GENESIS_VALUE_SETTER_ZK" "$GENESIS_DIR/value_setter_zk.json"

  print_ok "Genesis directory ready"
  echo ""
  echo "  value_setter_zk genesis:"
  python3 -m json.tool "$GENESIS_DIR/value_setter_zk.json" 2>/dev/null | sed 's/^/    /' \
    || sed 's/^/    /' < "$GENESIS_DIR/value_setter_zk.json"
fi

# ----- --generate-only: print artifacts and instructions, then exit ----------

if [ "$GENERATE_ONLY" -eq 1 ]; then
  print_header "Artifacts ready (--generate-only)"

  echo ""
  echo -e "  ${BOLD}Generated files:${NC}"
  echo "    TX payload (sov-cli JSON):  $TX_JSON"
  echo "    Genesis value_setter_zk:    $GENESIS_VALUE_SETTER_ZK"
  echo "    Full genesis directory:     $GENESIS_DIR"
  echo ""
  echo -e "  ${BOLD}How to run the rollup manually:${NC}"
  echo ""
  echo "    # 1. Build the rollup binary (if not already built):"
  echo "    SKIP_GUEST_BUILD=1 cargo build ${CARGO_PROFILE:---release} \\"
  echo "      --bin sov-demo-rollup --bin sov-cli \\"
  echo "      -p sov-demo-rollup --features nightstream"
  echo ""
  echo "    # 2. Start the rollup (mock DA):"
  echo "    RUST_LOG='info,sov_value_setter_zk=debug,sov_nightstream_adapter=debug' \\"
  echo "    RISC0_DEV_MODE=true \\"
  echo "      $SOV_ROLLUP \\"
  echo "      --da-layer mock \\"
  echo "      --rollup-config-path <YOUR_ROLLUP_CONFIG.toml> \\"
  echo "      --genesis-config-dir $GENESIS_DIR"
  echo ""
  echo "    # 3. Send the tx via sov-cli:"
  echo "    $SOV_CLI node set-url http://127.0.0.1:12346"
  echo "    $SOV_CLI keys import --skip-if-present --nickname mykey --path $KEY_PATH"
  echo "    $SOV_CLI transactions import from-file value-setter-zk \\"
  echo "      --chain-id $CHAIN_ID --max-fee 10000000000 --path $TX_JSON"
  echo "    NONCE=\$(python3 -c 'import time; print(int(time.time()*1000))')"
  echo "    $SOV_CLI node submit-batch \$NONCE by-nickname mykey"
  echo ""
  echo "    # 4. Check the result:"
  echo "    curl 'http://127.0.0.1:12346/ledger/txs/<TX_HASH>?children=1' | python3 -m json.tool"
  echo ""

  # Don't delete the work dir when --generate-only
  KEEP_ROLLUP=1
  exit 0
fi

# ----- Steps 4-5: Rollup config + start (skipped in --no-rollup mode) ------

if [ "$NO_ROLLUP" -eq 1 ]; then
  # --no-rollup: connect to an already-running rollup
  print_header "Connecting to external rollup at $ROLLUP_URL"

  if wait_for_rollup "$ROLLUP_URL" 10; then
    print_ok "External rollup is reachable"
  else
    print_fail "External rollup at $ROLLUP_URL is not reachable (healthcheck failed)"
    echo ""
    echo "  Make sure the rollup is running.  You can start one with:"
    echo "    examples/demo-rollup/run_rollup.sh"
    exit 1
  fi
else
  # ----- Step 4: Prepare rollup config ----------------------------------------

  print_header "Preparing rollup config"

  # When full-verify is on the sequencer must allow enough time for the Nightstream
  # verify-only pass (debug: ~2-5s, release: ~100ms).  Default of 350 ms is far too short.
  if [ "$FULL_VERIFY" -eq 1 ]; then
    BATCH_EXEC_LIMIT=120000   # 2 min (generous for debug builds)
  else
    BATCH_EXEC_LIMIT=2000     # 2 s is enough when skipping verification
  fi

  ROLLUP_CONFIG="$WORK_DIR/rollup_config.toml"
  cat > "$ROLLUP_CONFIG" << TOML
[da]
connection_string = "sqlite://$ROLLUP_DATA_DIR/mock_da.sqlite?mode=rwc"
sender_address = "0000000000000000000000000000000000000000000000000000000000000000"
finalization = 40
[da.block_producing.periodic]
block_time_ms = 2000

[storage]
path = "$ROLLUP_DATA_DIR"
state_cache_size = 1073741824

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
batch_execution_time_limit_millis = $BATCH_EXEC_LIMIT
num_cache_warmup_workers = 2
[sequencer.extension]
max_log_limit = 20000
TOML

  mkdir -p "$ROLLUP_DATA_DIR"
  print_ok "Rollup config (batch_execution_time_limit=${BATCH_EXEC_LIMIT}ms)"

  # ----- Step 5: Start the rollup --------------------------------------------

  print_header "Starting mock rollup"

  EXTRA_ENV=""
  if [ "$FULL_VERIFY" -eq 0 ]; then
    EXTRA_ENV="NIGHTSTREAM_SKIP_VERIFICATION=1"
    print_info "On-chain verification SKIPPED (use --full-verify to enable)"
  else
    print_info "On-chain verification ENABLED (full Nightstream verify-only)"
  fi

  env $EXTRA_ENV \
    RISC0_DEV_MODE=true \
    RUST_LOG="info,sov_value_setter_zk=debug,sov_nightstream_adapter=debug" \
    "$SOV_ROLLUP" \
    --da-layer mock \
    --rollup-config-path "$ROLLUP_CONFIG" \
    --genesis-config-dir "$GENESIS_DIR" \
    > "$WORK_DIR/rollup.log" 2>&1 &

  ROLLUP_PID=$!
  print_info "Rollup started (PID $ROLLUP_PID)"
  print_info "Waiting for rollup to be ready..."

  if wait_for_rollup "$ROLLUP_URL" 60; then
    print_ok "Rollup is ready at $ROLLUP_URL"
  else
    print_fail "Rollup did not start within 60 seconds"
    tail -30 "$WORK_DIR/rollup.log" | sed 's/^/    /'
    exit 1
  fi

  sleep 3
fi

# ----- Step 6: sov-cli import + submit -------------------------------------

print_header "Submitting transaction via sov-cli"

"$SOV_CLI" transactions clean 2>/dev/null || true

"$SOV_CLI" node set-url "$ROLLUP_URL" 2>/dev/null
print_ok "Node URL set"

"$SOV_CLI" keys import --skip-if-present --nickname "$KEY_NICKNAME" --path "$KEY_PATH" 2>/dev/null
print_ok "Key imported ($KEY_NICKNAME)"

"$SOV_CLI" transactions import from-file value-setter-zk \
  --chain-id "$CHAIN_ID" --max-fee 10000000000 --path "$TX_JSON" > /dev/null
print_ok "Transaction imported"

NONCE_MS=$(python3 -c "import time; print(int(time.time() * 1000))")
print_info "Generation nonce: $NONCE_MS"

# Submit (don't --wait-for-processing; we'll poll the status API instead)
SUBMIT_OUTPUT=$("$SOV_CLI" node submit-batch "$NONCE_MS" by-nickname "$KEY_NICKNAME" 2>&1) || true

# Extract the tx hash from the CLI output
TX_HASH=$(echo "$SUBMIT_OUTPUT" | grep -oE '0x[0-9a-f]{64}' | head -1 || true)
if [ -z "$TX_HASH" ]; then
  echo "$SUBMIT_OUTPUT" | sed 's/^/    /'
  print_fail "Could not extract tx hash from sov-cli output"
  exit 1
fi
print_ok "Transaction submitted: $TX_HASH"

# ----- Step 7: Poll ledger for tx receipt -----------------------------------

print_header "Polling transaction receipt"

# The sequencer's /sequencer/txs/:hash/status returns "unknown" for already-
# processed txs.  The ledger endpoint persists the receipt and is the reliable
# source of truth.
LEDGER_TX_URL="$ROLLUP_URL/ledger/txs/$TX_HASH"
POLL_TIMEOUT=180          # generous: full-verify in debug can take 60s+
POLL_INTERVAL=2
elapsed=0
TX_FINAL_STATUS=""
LEDGER_RESPONSE=""

while [ $elapsed -lt $POLL_TIMEOUT ]; do
  LEDGER_RESPONSE=$(curl -sf "${LEDGER_TX_URL}?children=1" 2>/dev/null || echo "")

  if [ -n "$LEDGER_RESPONSE" ]; then
    # Extract "result" from receipt:  "receipt":{"result":"successful", ...}
    TX_RECEIPT_RESULT=$(echo "$LEDGER_RESPONSE" \
      | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('receipt',{}).get('result',''))" 2>/dev/null || echo "")

    if [ "$TX_RECEIPT_RESULT" = "successful" ]; then
      TX_FINAL_STATUS="successful"
      break
    elif [ -n "$TX_RECEIPT_RESULT" ]; then
      # Any other non-empty result (e.g. "reverted")
      TX_FINAL_STATUS="$TX_RECEIPT_RESULT"
      break
    fi
    # Tx not yet in ledger -- keep polling
  fi

  sleep $POLL_INTERVAL
  elapsed=$((elapsed + POLL_INTERVAL))
  # Exponential back-off (cap at 10s)
  [ $POLL_INTERVAL -lt 10 ] && POLL_INTERVAL=$((POLL_INTERVAL * 2))
done

echo ""

if [ -z "$TX_FINAL_STATUS" ]; then
  print_fail "Timed out after ${POLL_TIMEOUT}s waiting for ledger receipt"
  print_info "Last response: $LEDGER_RESPONSE"
  FINAL_RESULT="FAIL"
elif [ "$TX_FINAL_STATUS" = "successful" ]; then
  print_ok "Ledger receipt: ${GREEN}successful${NC}"

  # Show event summary from the ledger response
  EVENT_SUMMARY=$(echo "$LEDGER_RESPONSE" \
    | python3 -c "
import sys, json
d = json.load(sys.stdin)
for ev in d.get('events', []):
    print(f\"  Event: {ev.get('key','')}  value={json.dumps(ev.get('value',''))}\")" 2>/dev/null || true)
  if [ -n "$EVENT_SUMMARY" ]; then
    echo "$EVENT_SUMMARY" | sed 's/^/  /'
  fi

  FINAL_RESULT="PASS"
else
  print_fail "Ledger receipt: $TX_FINAL_STATUS"
  echo "$LEDGER_RESPONSE" | python3 -m json.tool 2>/dev/null | sed 's/^/    /' \
    || echo "    $LEDGER_RESPONSE"
  FINAL_RESULT="FAIL"
fi

# ----- Step 8: Log evidence -------------------------------------------------

if [ "$NO_ROLLUP" -eq 1 ]; then
  # In --no-rollup mode we don't have the rollup log file.
  # Rely entirely on the ledger receipt for the verdict.
  print_header "Result (--no-rollup, no rollup log available)"
  echo ""
  print_info "Rollup is managed externally; check its terminal output for Nightstream log lines."
else
  print_header "Verification evidence from rollup log"

  echo ""
  echo "  Nightstream adapter log lines:"
  grep -iE "Nightstream:|nightstream_adapter|proof.*package|Deserializ" "$WORK_DIR/rollup.log" 2>/dev/null \
    | tail -20 | sed 's/^/    /' || echo "    (none)"
  echo ""

  echo "  Transaction execution log lines:"
  grep -E "apply_tx|Committed|Reverted|ValueSet|value_setter" "$WORK_DIR/rollup.log" 2>/dev/null \
    | tail -10 | sed 's/^/    /' || echo "    (none)"
  echo ""

  # ---------- Definitive verification verdict ----------
  if [ "$FULL_VERIFY" -eq 1 ]; then
    echo ""
    echo -e "  ${BOLD}Verification verdict (--full-verify):${NC}"

    if grep -q "Skipping verification" "$WORK_DIR/rollup.log" 2>/dev/null; then
      print_fail "NIGHTSTREAM_SKIP_VERIFICATION was unexpectedly set -- verification was NOT executed"
      FINAL_RESULT="FAIL"
    elif grep -q "proof verification PASSED" "$WORK_DIR/rollup.log" 2>/dev/null; then
      print_ok "Nightstream proof verification PASSED (verify-only succeeded)"
    elif grep -q "proof reconstruction failed" "$WORK_DIR/rollup.log" 2>/dev/null; then
      print_fail "Verification FAILED (proof reconstruction error)"
      grep "proof reconstruction failed" "$WORK_DIR/rollup.log" | tail -3 | sed 's/^/    /'
      FINAL_RESULT="FAIL"
    elif grep -q "proof verification failed" "$WORK_DIR/rollup.log" 2>/dev/null; then
      print_fail "Verification FAILED"
      grep "proof verification failed" "$WORK_DIR/rollup.log" | tail -3 | sed 's/^/    /'
      FINAL_RESULT="FAIL"
    elif grep -q "starting full proof verification" "$WORK_DIR/rollup.log" 2>/dev/null; then
      print_info "Verification started but no completion log found (may have timed out)"
      FINAL_RESULT="FAIL"
    else
      print_info "No Nightstream verification log lines found (tx may not have reached verification)"
      if [ "$FINAL_RESULT" != "PASS" ]; then FINAL_RESULT="FAIL"; fi
    fi
  else
    echo ""
    echo -e "  ${BOLD}Verification mode: skip (no --full-verify)${NC}"
    if grep -q "Skipping verification" "$WORK_DIR/rollup.log" 2>/dev/null; then
      print_ok "NIGHTSTREAM_SKIP_VERIFICATION active -- verification skipped as expected"
    fi
  fi
fi

# ----- Summary --------------------------------------------------------------

echo ""
echo "════════════════════════════════════════════════════"
if [ "$FINAL_RESULT" = "PASS" ]; then
  echo -e "${GREEN}${BOLD}  PASS  --  Nightstream E2E Rollup Test${NC}"
else
  echo -e "${RED}${BOLD}  FAIL  --  Nightstream E2E Rollup Test${NC}"
fi
echo "════════════════════════════════════════════════════"
echo ""
echo "  Value:         $TEST_VALUE"
echo "  TX hash:       $TX_HASH"
echo "  TX receipt:    $TX_FINAL_STATUS"
echo "  Full verify:   $([ "$FULL_VERIFY" -eq 1 ] && echo yes || echo no)"
if [ "$NO_ROLLUP" -eq 1 ]; then
  echo "  Rollup URL:    $ROLLUP_URL (external)"
else
  echo "  Rollup log:    $WORK_DIR/rollup.log"
fi
echo ""

if [ "$NO_ROLLUP" -eq 1 ]; then
  echo "  Useful commands:"
  echo "    curl '$ROLLUP_URL/ledger/txs/$TX_HASH?children=1' | python3 -m json.tool"
  echo ""
elif [ "$KEEP_ROLLUP" -eq 1 ]; then
  echo "  Rollup still running. Useful commands:"
  echo "    curl '$ROLLUP_URL/ledger/txs/$TX_HASH?children=1' | python3 -m json.tool"
  echo "    kill $ROLLUP_PID"
  echo ""
fi

[ "$FINAL_RESULT" = "PASS" ] && exit 0 || exit 1
