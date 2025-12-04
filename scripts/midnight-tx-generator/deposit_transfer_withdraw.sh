#!/bin/bash
set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

GENERATOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$GENERATOR_DIR/../.." && pwd)"

PRIVATE_KEY_FILE="${PRIVATE_KEY_FILE:-$REPO_ROOT/examples/test-data/keys/tx_signer_private_key.json}"
RECIPIENT="${RECIPIENT:-sov1v870parxhssv5wyz634wqlt9yflrrnawlwzjhj8409q4yevcj3s}"
DEPOSIT_AMOUNT="${DEPOSIT_AMOUNT:-1000}"
TRANSFER_OUT1="${TRANSFER_OUT1:-600}"
TRANSFER_OUT2="${TRANSFER_OUT2:-400}"
WITHDRAW_AMOUNT="${WITHDRAW_AMOUNT:-200}"

# Determine a base nonce: prefer node-reported latest nonce + 1, fallback to local monotonic .last_nonce, then time
NONCE_STATE_FILE="$GENERATOR_DIR/.last_nonce"
BASE_NONCE=$(date +%s)

# Try to fetch latest from node for this key
NODE_API_URL="${NODE_API_URL:-http://localhost:12346}"
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build --bin fetch-nonce 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"
LATEST_ON_NODE=$("$GENERATOR_DIR/target/debug/fetch-nonce" "$NODE_API_URL" "$PRIVATE_KEY_FILE" 2>/dev/null || echo "")

if [ -n "$LATEST_ON_NODE" ]; then
  # next usable nonce
  BASE_NONCE=$((LATEST_ON_NODE + 1))
fi

if [ -f "$NONCE_STATE_FILE" ]; then
  LAST_NONCE=$(cat "$NONCE_STATE_FILE" | tr -d '\n' || echo 0)
  # Bump by 3 to cover deposit (+0), transfer (+1), withdraw (+2)
  if [ "$BASE_NONCE" -le "$LAST_NONCE" ]; then
    BASE_NONCE=$((LAST_NONCE + 3))
  fi
fi

# Allow override via env NONCE; otherwise use computed BASE_NONCE
NONCE="${NONCE:-$BASE_NONCE}"

# Optional: fund the sender before shielded deposit
FUNDER_KEY_FILE="${FUNDER_KEY_FILE:-$REPO_ROOT/examples/test-data/keys/token_deployer_private_key.json}"
FUND_AMOUNT="${FUND_AMOUNT:-1000}" # clear tokens to send to sender

# Endpoint - always send to worker (proof verifier service which forwards to sequencer)
VERIFIER_ENDPOINT="${VERIFIER_ENDPOINT:-http://localhost:8080/midnight-privacy}"

echo -e "${BLUE}=== Midnight Privacy: Full Lifecycle Demo ===${NC}\n"
echo "Flow:"
echo "  1. Deposit $DEPOSIT_AMOUNT (transparent → shielded)"
echo "  2. Transfer: Split into $TRANSFER_OUT1 + $TRANSFER_OUT2 (shielded → shielded)"
echo "  3. Withdraw $WITHDRAW_AMOUNT from first output (shielded → transparent)"
echo ""
echo "Parameters:"
echo "  Nonce: $NONCE"
echo "  Worker:    $VERIFIER_ENDPOINT"
echo ""

# Build generators if needed
if [ ! -f "$GENERATOR_DIR/target/debug/midnight-deposit-generator" ]; then
    echo -e "${YELLOW}Building generators...${NC}"
    cd "$GENERATOR_DIR"
    SKIP_GUEST_BUILD=1 cargo build --bin midnight-deposit-generator 2>&1 | grep -E "Compiling|Finished" || true
    cd "$REPO_ROOT"
    echo ""
fi

# Step 0: fund sender if requested (native Rust funder)
if [ -n "$FUND_AMOUNT" ] && [ "$FUND_AMOUNT" != "0" ]; then
  echo -e "${YELLOW}Step 0: Funding sender with $FUND_AMOUNT tokens${NC}"
  cd "$GENERATOR_DIR"
  SKIP_GUEST_BUILD=1 cargo build --bin fund 2>&1 | grep -E "Compiling|Finished" || true
  cd "$REPO_ROOT"
  FUND_NONCE=""
  if [ -f "$NONCE_STATE_FILE" ]; then
    LAST_NONCE=$(cat "$NONCE_STATE_FILE" | tr -d '\n' || echo 0)
    if [[ "$LAST_NONCE" =~ ^[0-9]+$ ]]; then
      FUND_NONCE=$((LAST_NONCE + 1))
    fi
  fi
  env \
    NODE_API_URL="${NODE_API_URL:-http://localhost:12346}" \
    RECIPIENT="$RECIPIENT" \
    FUND_AMOUNT="$FUND_AMOUNT" \
    FUNDER_KEY_FILE="$FUNDER_KEY_FILE" \
    ${FUND_NONCE:+FUND_NONCE="$FUND_NONCE"} \
    "$GENERATOR_DIR/target/debug/fund"
  echo -e "${GREEN}✓ Funding submitted via native funder${NC}\n"
fi

#############################################################################
# STEP 1: DEPOSIT - Put money INTO the privacy pool
#############################################################################
echo -e "${YELLOW}━━━ Step 1: Deposit ($DEPOSIT_AMOUNT tokens) ━━━${NC}"
export DEPOSIT_AMOUNT NONCE PRIVATE_KEY_FILE
cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/midnight-deposit-generator" "midnight_deposit_tx.bin" > /tmp/deposit.log
cd "$REPO_ROOT"

# Send deposit to verifier service (it forwards to sequencer)
DEPOSIT_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_deposit_tx.json" \
  "$VERIFIER_ENDPOINT")

echo "$DEPOSIT_RESPONSE" | jq '.' 2>/dev/null || echo "$DEPOSIT_RESPONSE"

DEPOSIT_SUCCESS=$(echo "$DEPOSIT_RESPONSE" | jq -r '.success // empty')
if [ "$DEPOSIT_SUCCESS" != "true" ]; then
    echo -e "${RED}✗ Deposit failed${NC}"
    exit 1
fi

SEQUENCER_RESPONSE=$(echo "$DEPOSIT_RESPONSE" | jq -c '.sequencer_response // empty')
if [ -z "$SEQUENCER_RESPONSE" ] || [ "$SEQUENCER_RESPONSE" = "null" ]; then
    echo -e "${RED}✗ Sequencer response missing from deposit result${NC}"
    exit 1
fi

# Extract commitment from deposit response and look up authoritative position/root via REST
COMMITMENT=$(echo "$SEQUENCER_RESPONSE" | jq -c '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.commitment')
if [ -z "$COMMITMENT" ] || [ "$COMMITMENT" = "null" ]; then
    echo -e "${RED}✗ Failed to extract commitment from deposit response${NC}"
    exit 1
fi

NOTE_POSITION=""
ANCHOR_ROOT=""
for attempt in $(seq 1 60); do
    NOTES_RESP=$(curl -s "${NODE_API_URL}/modules/midnight-privacy/notes?limit=200&reverse=true")
    NOTE_POSITION=$(echo "$NOTES_RESP" | jq --argjson target "$COMMITMENT" -r '.notes[] | select(.commitment == $target) | .position' | head -n1 | tr -d '\n')
    ANCHOR_ROOT=$(echo "$NOTES_RESP" | jq -c '.current_root // empty')
    if [ -n "$NOTE_POSITION" ] && [ "$NOTE_POSITION" != "null" ]; then
        break
    fi
    sleep 1
done

if [ -z "$NOTE_POSITION" ] || [ "$NOTE_POSITION" = "null" ] || ! [[ "$NOTE_POSITION" =~ ^[0-9]+$ ]]; then
    echo -e "${RED}✗ Failed to locate valid note position in /modules/midnight-privacy/notes${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Deposit successful${NC}"
echo "  Created: Note@pos$NOTE_POSITION ($DEPOSIT_AMOUNT tokens)"
if [ -n "$ANCHOR_ROOT" ] && [ "$ANCHOR_ROOT" != "null" ]; then
    echo "  Anchor: $(echo $ANCHOR_ROOT | jq -r 'if type == "array" then (.[0:8] | map(tostring) | join(",")) else . end')"
else
    echo "  Anchor: (not returned from notes endpoint)"
fi
echo ""

# Wait for confirmation
echo "Waiting 3 seconds..."
sleep 3
echo ""

#############################################################################
# STEP 2: TRANSFER - Move money WITHIN the privacy pool (pure shielded)
#############################################################################
echo -e "${YELLOW}━━━ Step 2: Transfer (split $DEPOSIT_AMOUNT → $TRANSFER_OUT1 + $TRANSFER_OUT2) ━━━${NC}"

# Re-fetch latest nonce from node to avoid stale generation
LATEST_ON_NODE=$("$GENERATOR_DIR/target/debug/fetch-nonce" "$NODE_API_URL" "$PRIVATE_KEY_FILE" 2>/dev/null || echo "")
if [ -n "$LATEST_ON_NODE" ]; then
  TRANSFER_NONCE=$((LATEST_ON_NODE + 1))
else
  TRANSFER_NONCE=$((NONCE + 1))
fi

# Load note details
NOTE_DETAILS_FILE="$GENERATOR_DIR/midnight_note_details.json"
NOTE_DOMAIN=$(cat "$NOTE_DETAILS_FILE" | jq -r '.domain')
NOTE_VALUE=$(cat "$NOTE_DETAILS_FILE" | jq -r '.amount')
NOTE_RHO=$(cat "$NOTE_DETAILS_FILE" | jq -r '.rho')
NOTE_RECIPIENT=$(cat "$NOTE_DETAILS_FILE" | jq -r '.recipient')
NOTE_NF_KEY=$(cat "$NOTE_DETAILS_FILE" | jq -r '.nf_key')

# Build transfer generator
SKIP_GUEST_BUILD=1 cargo build --bin transfer-generator 2>&1 | grep -E "Compiling|Finished" || true

# Set environment and generate transfer
export NOTE_DOMAIN NOTE_VALUE NOTE_RHO NOTE_RECIPIENT NOTE_NF_KEY
export NOTE_POSITION ANCHOR_ROOT
export TRANSFER_OUT1 TRANSFER_OUT2
export NONCE=$TRANSFER_NONCE
export PRIVATE_KEY_FILE
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm}"
export LIGERO_PACKING="${LIGERO_PACKING:-8192}"

cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/transfer-generator" 2>&1 | tee /tmp/transfer.log
cd "$REPO_ROOT"

# Send transfer to verifier service (it forwards to sequencer)
TRANSFER_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_transfer_tx.json" \
  "$VERIFIER_ENDPOINT")

echo "$TRANSFER_RESPONSE" | jq '.' 2>/dev/null || echo "$TRANSFER_RESPONSE"

TRANSFER_SUCCESS=$(echo "$TRANSFER_RESPONSE" | jq -r '.success // empty')
if [ "$TRANSFER_SUCCESS" != "true" ]; then
    echo -e "${RED}✗ Transfer failed${NC}"
    exit 1
fi

SEQUENCER_TRANSFER_RESPONSE=$(echo "$TRANSFER_RESPONSE" | jq -c '.sequencer_response // empty')
if [ -z "$SEQUENCER_TRANSFER_RESPONSE" ] || [ "$SEQUENCER_TRANSFER_RESPONSE" = "null" ]; then
    echo -e "${RED}✗ Sequencer response missing from transfer result${NC}"
    exit 1
fi

# Extract new positions from transfer response (lookup via /notes)
TRANSFER_EVENTS=$(echo "$SEQUENCER_TRANSFER_RESPONSE" | jq -c '[.events[] | select(.key == "ValueMidnightPrivacy/NoteCreated")]')
OUT1_COMMITMENT=$(echo "$TRANSFER_EVENTS" | jq -c '.[0].value.note_created.commitment')
OUT2_COMMITMENT=$(echo "$TRANSFER_EVENTS" | jq -c '.[1].value.note_created.commitment')

OUT1_POSITION=""
OUT2_POSITION=""
TRANSFER_ROOT=""
for attempt in $(seq 1 60); do
    NOTES_RESP=$(curl -s "${NODE_API_URL}/modules/midnight-privacy/notes?limit=200&reverse=true")
    OUT1_POSITION=$(echo "$NOTES_RESP" | jq --argjson target "$OUT1_COMMITMENT" -r '.notes[] | select(.commitment == $target) | .position' | head -n1 | tr -d '\n')
    OUT2_POSITION=$(echo "$NOTES_RESP" | jq --argjson target "$OUT2_COMMITMENT" -r '.notes[] | select(.commitment == $target) | .position' | head -n1 | tr -d '\n')
    TRANSFER_ROOT=$(echo "$NOTES_RESP" | jq -c '.current_root // empty')
    if [ -n "$OUT1_POSITION" ] && [ "$OUT1_POSITION" != "null" ] && [[ "$OUT1_POSITION" =~ ^[0-9]+$ ]] \
       && [ -n "$OUT2_POSITION" ] && [ "$OUT2_POSITION" != "null" ] && [[ "$OUT2_POSITION" =~ ^[0-9]+$ ]]; then
        break
    fi
    sleep 1
done

if [ -z "$OUT1_POSITION" ] || [ "$OUT1_POSITION" = "null" ] || ! [[ "$OUT1_POSITION" =~ ^[0-9]+$ ]] \
   || [ -z "$OUT2_POSITION" ] || [ "$OUT2_POSITION" = "null" ] || ! [[ "$OUT2_POSITION" =~ ^[0-9]+$ ]]; then
    echo -e "${RED}✗ Failed to locate transfer output positions in /modules/midnight-privacy/notes${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Transfer successful${NC}"
echo "  Consumed: Note@pos$NOTE_POSITION ($DEPOSIT_AMOUNT tokens)"
echo "  Created: Note@pos$OUT1_POSITION ($TRANSFER_OUT1 tokens)"
echo "  Created: Note@pos$OUT2_POSITION ($TRANSFER_OUT2 tokens)"
echo "  Status: All value stays shielded (pure privacy)"
echo ""

# Wait for confirmation
echo "Waiting 3 seconds..."
sleep 3
echo ""

#############################################################################
# STEP 3: WITHDRAW - Take money OUT of the privacy pool
#############################################################################
echo -e "${YELLOW}━━━ Step 3: Withdraw ($WITHDRAW_AMOUNT from Note@pos$OUT1_POSITION) ━━━${NC}"

# Re-fetch latest nonce from node before withdraw
LATEST_ON_NODE=$("$GENERATOR_DIR/target/debug/fetch-nonce" "$NODE_API_URL" "$PRIVATE_KEY_FILE" 2>/dev/null || echo "")
if [ -n "$LATEST_ON_NODE" ]; then
  WITHDRAW_NONCE=$((LATEST_ON_NODE + 1))
else
  WITHDRAW_NONCE=$((NONCE + 2))
fi

# Load first output details
OUT1_DETAILS_FILE="$GENERATOR_DIR/midnight_transfer_out1_details.json"
OUT1_DOMAIN=$(cat "$OUT1_DETAILS_FILE" | jq -r '.domain')
OUT1_VALUE=$(cat "$OUT1_DETAILS_FILE" | jq -r '.amount')
OUT1_RHO=$(cat "$OUT1_DETAILS_FILE" | jq -r '.rho')
OUT1_RECIPIENT=$(cat "$OUT1_DETAILS_FILE" | jq -r '.recipient')
OUT1_NF_KEY=$(cat "$OUT1_DETAILS_FILE" | jq -r '.nf_key')

# Create withdrawal generator

# Replace placeholder with actual repo root path

SKIP_GUEST_BUILD=1 cargo build --bin withdraw-generator 2>&1 | grep -E "Compiling|Finished" || true

# Set environment and generate withdrawal
export OUT1_DOMAIN OUT1_VALUE OUT1_RHO OUT1_RECIPIENT OUT1_NF_KEY
export OUT1_POSITION TRANSFER_ROOT
export WITHDRAW_AMOUNT RECIPIENT
export NONCE=$WITHDRAW_NONCE
export PRIVATE_KEY_FILE
export LIGERO_PROGRAM_PATH LIGERO_PACKING

cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/withdraw-generator" 2>&1 | tee /tmp/withdraw.log
cd "$REPO_ROOT"

# Send withdrawal to verifier service (it forwards to sequencer)
WITHDRAW_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_withdraw_tx.json" \
  "$VERIFIER_ENDPOINT")

echo "$WITHDRAW_RESPONSE" | jq '.' 2>/dev/null || echo "$WITHDRAW_RESPONSE"

WITHDRAW_SUCCESS=$(echo "$WITHDRAW_RESPONSE" | jq -r '.success // empty')
if [ "$WITHDRAW_SUCCESS" != "true" ]; then
    echo -e "${RED}✗ Withdrawal failed${NC}"
    exit 1
fi

SEQUENCER_WITHDRAW_RESPONSE=$(echo "$WITHDRAW_RESPONSE" | jq -c '.sequencer_response // empty')
if [ -z "$SEQUENCER_WITHDRAW_RESPONSE" ] || [ "$SEQUENCER_WITHDRAW_RESPONSE" = "null" ]; then
    echo -e "${RED}✗ Sequencer response missing from withdraw result${NC}"
    exit 1
fi

CHANGE_POSITION=$(echo "$SEQUENCER_WITHDRAW_RESPONSE" | jq -r '.events[] | select(.key == "ValueMidnightPrivacy/NoteCreated") | .value.note_created.position')

echo -e "${GREEN}✓ Withdrawal successful${NC}"
echo "  Consumed: Note@pos$OUT1_POSITION ($TRANSFER_OUT1 tokens)"
echo "  Withdrew: $WITHDRAW_AMOUNT tokens (transparent to $RECIPIENT)"
echo "  Created: Note@pos$CHANGE_POSITION ($((TRANSFER_OUT1 - WITHDRAW_AMOUNT)) tokens change)"
echo ""

cd "$REPO_ROOT"

#############################################################################
# SUMMARY
#############################################################################
echo -e "${BLUE}=== Summary ===${NC}"
echo ""
echo "Full Privacy Lifecycle Completed:"
echo "  1. Deposit:  $DEPOSIT_AMOUNT → Note@pos$NOTE_POSITION"
echo "  2. Transfer: Note@pos$NOTE_POSITION($DEPOSIT_AMOUNT) → Note@pos$OUT1_POSITION($TRANSFER_OUT1) + Note@pos$OUT2_POSITION($TRANSFER_OUT2)"
echo "  3. Withdraw: Note@pos$OUT1_POSITION($TRANSFER_OUT1) → $WITHDRAW_AMOUNT(transparent) + Note@pos$CHANGE_POSITION($((TRANSFER_OUT1 - WITHDRAW_AMOUNT)))"
echo ""
echo "Final State:"
echo "  • Transparent balance: +$WITHDRAW_AMOUNT tokens (withdrawn)"
echo "  • Shielded pool: Note@pos$OUT2_POSITION($TRANSFER_OUT2) + Note@pos$CHANGE_POSITION($((TRANSFER_OUT1 - WITHDRAW_AMOUNT))) = $((TRANSFER_OUT2 + TRANSFER_OUT1 - WITHDRAW_AMOUNT)) tokens"
echo ""
echo -e "${GREEN}✓ All transactions successful!${NC}"

# Persist the last used nonce so future runs remain monotonic
echo $((NONCE + 2)) > "$NONCE_STATE_FILE"
