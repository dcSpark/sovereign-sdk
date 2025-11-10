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
NONCE_STATE_FILE="$GENERATOR_DIR/.last_nonce_direct"
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

# Optional viewer full viewing keys (comma-separated hex). Default demo key emits encrypted note payloads.
DEFAULT_VIEW_FVK="0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
VIEWER_FVKS="${VIEWER_FVKS:-$DEFAULT_VIEW_FVK}"

# Endpoint - send directly to sequencer
SEQUENCER_ENDPOINT="${SEQUENCER_ENDPOINT:-http://localhost:12346/sequencer/txs}"

echo -e "${BLUE}=== Midnight Privacy: Deposit + Withdraw Flow (DIRECT TO SEQUENCER) ===${NC}\n"
echo "Parameters:"
echo "  Deposit: $DEPOSIT_AMOUNT"
echo "  Withdraw: $WITHDRAW_AMOUNT"
echo "  Change: $((DEPOSIT_AMOUNT - WITHDRAW_AMOUNT)) (stays shielded)"
echo "  Nonce: $NONCE"
echo "  Sequencer Endpoint: $SEQUENCER_ENDPOINT"
echo "  Viewer FVKs: $VIEWER_FVKS"
echo ""

# Build generators if needed
echo -e "${YELLOW}Building generators...${NC}"
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build --bin midnight-deposit-generator --bin withdraw-with-tree 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"
echo ""

# Step 1: Generate and send deposit
echo -e "${YELLOW}Step 1: Deposit${NC}"
export DEPOSIT_AMOUNT NONCE PRIVATE_KEY_FILE VIEWER_FVKS
cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/midnight-deposit-generator" "midnight_deposit_tx.bin" > /tmp/deposit_direct.log
cd "$REPO_ROOT"

# Send deposit directly to sequencer (same format as proof verifier)
echo -e "${BLUE}Sending deposit directly to sequencer...${NC}"
DEPOSIT_SUBMIT_START=$(date +%s%N)
DEPOSIT_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_deposit_tx.json" \
  "$SEQUENCER_ENDPOINT")
DEPOSIT_SUBMIT_END=$(date +%s%N)
DEPOSIT_SUBMIT_MS=$(( (DEPOSIT_SUBMIT_END - DEPOSIT_SUBMIT_START) / 1000000 ))

echo "$DEPOSIT_RESPONSE" | jq '.' 2>/dev/null || echo "$DEPOSIT_RESPONSE"

# Check if response indicates an error (400 status or error field)
DEPOSIT_STATUS=$(echo "$DEPOSIT_RESPONSE" | jq -r '.status // empty' 2>/dev/null)
if [ "$DEPOSIT_STATUS" = "400" ]; then
    ERROR_MSG=$(echo "$DEPOSIT_RESPONSE" | jq -r '.message // empty')
    echo -e "${RED}✗ Deposit failed: $ERROR_MSG${NC}"
    exit 1
fi

# For direct sequencer calls, the response is the sequencer response directly (no wrapper)
SEQUENCER_RESPONSE="$DEPOSIT_RESPONSE"

# Extract position and anchor root from deposit response
NOTE_POSITION=$(echo "$SEQUENCER_RESPONSE" | jq -r '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.position' 2>/dev/null)
ANCHOR_ROOT=$(echo "$SEQUENCER_RESPONSE" | jq -c '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.new_root' 2>/dev/null)

if [ -z "$NOTE_POSITION" ] || [ "$NOTE_POSITION" = "null" ]; then
    echo -e "${RED}✗ Failed to extract note position from deposit response${NC}"
    echo -e "${RED}   This may indicate the transaction was rejected by the sequencer${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Deposit sent${NC}"
echo "  Note position: $NOTE_POSITION"
echo "  Anchor root: $(echo $ANCHOR_ROOT | jq -r 'if type == "array" then (.[0:8] | map(tostring) | join(",")) else . end')"
echo "  Sequencer response time: ${DEPOSIT_SUBMIT_MS}ms"
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
"$GENERATOR_DIR/target/debug/withdraw-with-tree" 2>&1 | tee /tmp/withdraw_direct.log
cd "$REPO_ROOT"

# Send withdrawal directly to sequencer (same format as proof verifier)
echo -e "${BLUE}Sending withdrawal directly to sequencer...${NC}"
WITHDRAW_SUBMIT_START=$(date +%s%N)
WITHDRAW_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_withdraw_tx.json" \
  "$SEQUENCER_ENDPOINT")
WITHDRAW_SUBMIT_END=$(date +%s%N)
WITHDRAW_SUBMIT_MS=$(( (WITHDRAW_SUBMIT_END - WITHDRAW_SUBMIT_START) / 1000000 ))

echo "$WITHDRAW_RESPONSE" | jq '.' 2>/dev/null || echo "$WITHDRAW_RESPONSE"

# Check if response indicates an error (400 status or error field)
WITHDRAW_STATUS=$(echo "$WITHDRAW_RESPONSE" | jq -r '.status // empty' 2>/dev/null)
if [ "$WITHDRAW_STATUS" = "400" ]; then
    ERROR_MSG=$(echo "$WITHDRAW_RESPONSE" | jq -r '.message // empty')
    echo -e "${RED}✗ Withdrawal failed: $ERROR_MSG${NC}"
    exit 1
fi

# Check if withdrawal was successful
# For direct sequencer calls, success is indicated by having events in the response
WITHDRAW_EVENTS=$(echo "$WITHDRAW_RESPONSE" | jq -r '.events // empty' 2>/dev/null)
if [ -z "$WITHDRAW_EVENTS" ] || [ "$WITHDRAW_EVENTS" = "null" ]; then
    echo -e "${RED}✗ Withdrawal failed or no events in response${NC}"
    echo -e "${RED}   This may indicate the transaction was rejected by the sequencer${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Withdrawal sent${NC}"
echo "  Sequencer response time: ${WITHDRAW_SUBMIT_MS}ms"
echo ""

echo -e "${GREEN}=== Success! ===${NC}"
echo "  Deposited: $DEPOSIT_AMOUNT"
echo "  Withdrew: $WITHDRAW_AMOUNT (transparent)"
echo "  Change: $((DEPOSIT_AMOUNT - WITHDRAW_AMOUNT)) (stays in shielded pool)"
echo ""
echo -e "${CYAN}Sequencer Timings:${NC}"
echo "  Deposit submission: ${DEPOSIT_SUBMIT_MS}ms"
echo "  Withdraw submission: ${WITHDRAW_SUBMIT_MS}ms"

# Persist the last used nonce so future runs remain monotonic
echo $((NONCE + 1)) > "$NONCE_STATE_FILE"

# Write timing data for comparison script
cat > "$GENERATOR_DIR/.direct_timings" <<EOF
DEPOSIT_SUBMIT_MS=$DEPOSIT_SUBMIT_MS
WITHDRAW_SUBMIT_MS=$WITHDRAW_SUBMIT_MS
EOF

