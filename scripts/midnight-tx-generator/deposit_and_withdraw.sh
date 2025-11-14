#!/bin/bash
set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

GENERATOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$GENERATOR_DIR/../.." && pwd)"

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
  # Bump by 2 to cover deposit (+0) and withdraw (+1)
  if [ "$BASE_NONCE" -le "$LAST_NONCE" ]; then
    BASE_NONCE=$((LAST_NONCE + 2))
  fi
fi

# Allow override via env NONCE; otherwise use computed BASE_NONCE
NONCE="${NONCE:-$BASE_NONCE}"
DEPOSIT_AMOUNT="${DEPOSIT_AMOUNT:-100}"
WITHDRAW_AMOUNT="${WITHDRAW_AMOUNT:-50}"
PRIVATE_KEY_FILE="${PRIVATE_KEY_FILE:-$REPO_ROOT/examples/test-data/keys/tx_signer_private_key.json}"
RECIPIENT="${RECIPIENT:-sov1v870parxhssv5wyz634wqlt9yflrrnawlwzjhj8409q4yevcj3s}"

# Endpoint - proof verifier service orchestrates sequencer submissions
VERIFIER_ENDPOINT="${VERIFIER_ENDPOINT:-http://localhost:8080/midnight-privacy}"

echo -e "${BLUE}=== Midnight Privacy: Deposit + Withdraw Flow ===${NC}\n"
echo "Parameters:"
echo "  Deposit: $DEPOSIT_AMOUNT"
echo "  Withdraw: $WITHDRAW_AMOUNT"
echo "  Change: $((DEPOSIT_AMOUNT - WITHDRAW_AMOUNT)) (stays shielded)"
echo "  Nonce: $NONCE"
echo "  Proof Verifier: $VERIFIER_ENDPOINT"
echo ""

# Build generators if needed
echo -e "${YELLOW}Building generators...${NC}"
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build --bin midnight-deposit-generator --bin withdraw-with-tree 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"
echo ""

# Step 1: Generate and send deposit
echo -e "${YELLOW}Step 1: Deposit${NC}"
export DEPOSIT_AMOUNT NONCE PRIVATE_KEY_FILE
cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/midnight-deposit-generator" "midnight_deposit_tx.bin" > /tmp/deposit.log
cd "$REPO_ROOT"

# Send deposit to proof verifier (which forwards to sequencer)
DEPOSIT_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_deposit_tx.json" \
  "$VERIFIER_ENDPOINT")

echo "$DEPOSIT_RESPONSE" | jq '.' 2>/dev/null || echo "$DEPOSIT_RESPONSE"

DEPOSIT_SUCCESS=$(echo "$DEPOSIT_RESPONSE" | jq -r '.success // empty')
if [ "$DEPOSIT_SUCCESS" != "true" ]; then
    ERROR_MSG=$(echo "$DEPOSIT_RESPONSE" | jq -r '.error // empty')
    if [ -n "$ERROR_MSG" ] && [ "$ERROR_MSG" != "null" ]; then
        echo -e "${RED}✗ Deposit failed: $ERROR_MSG${NC}"
    else
        echo -e "${RED}✗ Deposit failed${NC}"
    fi
    exit 1
fi

SEQUENCER_RESPONSE=$(echo "$DEPOSIT_RESPONSE" | jq -c '.sequencer_response // empty')
if [ -z "$SEQUENCER_RESPONSE" ] || [ "$SEQUENCER_RESPONSE" = "null" ]; then
    echo -e "${RED}✗ Sequencer response missing from deposit result${NC}"
    exit 1
fi

# Extract position and anchor root from deposit response
NOTE_POSITION=$(echo "$SEQUENCER_RESPONSE" | jq -r '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.position')
ANCHOR_ROOT=$(echo "$SEQUENCER_RESPONSE" | jq -c '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.new_root')

if [ -z "$NOTE_POSITION" ] || [ "$NOTE_POSITION" = "null" ]; then
    echo -e "${RED}✗ Failed to extract note position from deposit response${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Deposit sent${NC}"
echo "  Note position: $NOTE_POSITION"
echo "  Anchor root: $(echo $ANCHOR_ROOT | jq -r 'if type == "array" then (.[0:8] | map(tostring) | join(",")) else . end')"
echo ""

# Wait for confirmation
echo -e "${YELLOW}Waiting 3 seconds for confirmation...${NC}"
sleep 3
echo ""

# Step 2: Generate and send withdrawal
echo -e "${YELLOW}Step 2: Withdraw${NC}"

# Increment nonce for withdrawal (prefer node latest + 1)
LATEST_ON_NODE=$("$GENERATOR_DIR/target/debug/fetch-nonce" "${NODE_API_URL:-http://localhost:12346}" "$PRIVATE_KEY_FILE" 2>/dev/null || echo "")
if [ -n "$LATEST_ON_NODE" ]; then
  WITHDRAW_NONCE=$((LATEST_ON_NODE + 1))
else
  WITHDRAW_NONCE=$((NONCE + 1))
fi

# Load note details
NOTE_DETAILS_FILE="$GENERATOR_DIR/midnight_note_details.json"
NOTE_DOMAIN=$(cat "$NOTE_DETAILS_FILE" | jq -r '.domain')
NOTE_VALUE=$(cat "$NOTE_DETAILS_FILE" | jq -r '.amount')
NOTE_RHO=$(cat "$NOTE_DETAILS_FILE" | jq -r '.rho')
NOTE_RECIPIENT=$(cat "$NOTE_DETAILS_FILE" | jq -r '.recipient')
NOTE_NF_KEY=$(cat "$NOTE_DETAILS_FILE" | jq -r '.nf_key')

# Build the withdrawal generator if needed and run it
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build --bin withdraw-with-tree 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"

# Set environment and generate withdrawal
# Set LIGERO environment for proof generation
export LIGERO_PROGRAM_PATH="$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"
export LIGERO_PACKING=8192
if [[ "$OSTYPE" == "darwin"* ]]; then
    export LIGERO_SHADER_PATH="$REPO_ROOT/crates/adapters/ligero/bins/macos/shader"
else
    export LIGERO_SHADER_PATH="$REPO_ROOT/crates/adapters/ligero/bins/linux-amd64/shader"
fi

export NOTE_DOMAIN NOTE_VALUE NOTE_RHO NOTE_RECIPIENT NOTE_NF_KEY
export WITHDRAW_AMOUNT RECIPIENT
export NOTE_POSITION
export ANCHOR_ROOT
export NONCE=$WITHDRAW_NONCE
export PRIVATE_KEY_FILE

cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/withdraw-with-tree" 2>&1 | tee /tmp/withdraw.log
cd "$REPO_ROOT"

# Send withdrawal to proof verifier (which forwards to sequencer)
WITHDRAW_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_withdraw_tx.json" \
  "$VERIFIER_ENDPOINT")

echo "$WITHDRAW_RESPONSE" | jq '.' 2>/dev/null || echo "$WITHDRAW_RESPONSE"

WITHDRAW_SUCCESS=$(echo "$WITHDRAW_RESPONSE" | jq -r '.success // empty')
if [ "$WITHDRAW_SUCCESS" != "true" ]; then
    ERROR_MSG=$(echo "$WITHDRAW_RESPONSE" | jq -r '.error // empty')
    if [ -n "$ERROR_MSG" ] && [ "$ERROR_MSG" != "null" ]; then
        echo -e "${RED}✗ Withdrawal failed: $ERROR_MSG${NC}"
    else
        echo -e "${RED}✗ Withdrawal failed${NC}"
    fi
    exit 1
fi

echo -e "${GREEN}✓ Withdrawal sent${NC}\n"
echo -e "${GREEN}=== Success! ===${NC}"
echo "  Deposited: $DEPOSIT_AMOUNT"
echo "  Withdrew: $WITHDRAW_AMOUNT (transparent)"
echo "  Change: $((DEPOSIT_AMOUNT - WITHDRAW_AMOUNT)) (stays in shielded pool)"

# Persist the last used nonce so future runs remain monotonic
echo $((NONCE + 1)) > "$NONCE_STATE_FILE"
