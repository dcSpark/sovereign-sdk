#!/bin/bash
set -e
set -o pipefail

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

# Use remote prover service by default (daemon mode - faster for multiple proofs)
# Set PROVER_SERVICE_URL="" to use local prover binary instead
PROVER_SERVICE_URL="${PROVER_SERVICE_URL:-http://127.0.0.1:1313}"

# Best-effort: auto-discover the Ligero portable `webgpu_prover` binary from the Cargo git checkout.
# This avoids relying on PATH while keeping Sovereign free of extra Ligero env vars.
# Skip discovery if using remote prover service.
discover_ligero_prover_bin() {
  command -v python3 >/dev/null 2>&1 || return 1
  python3 - <<'PY'
import glob
import os
import platform
import sys

os_name = platform.system()
arch = platform.machine()

if os_name == "Darwin" and arch == "arm64":
    plat = "macos-arm64"
elif os_name == "Linux" and arch in ("x86_64", "amd64"):
    plat = "linux-amd64"
elif os_name == "Linux" and arch in ("aarch64", "arm64"):
    plat = "linux-arm64"
else:
    sys.exit(2)

pattern = os.path.expanduser(
    f"~/.cargo/git/checkouts/ligero-prover-*/**/utils/portable-binaries/{plat}/bin/webgpu_prover"
)
cands = glob.glob(pattern, recursive=True)

def ok(p: str) -> bool:
    d = os.path.dirname(p)
    return os.path.isfile(p) and os.path.isfile(os.path.join(d, "webgpu_verifier"))

cands = [p for p in cands if ok(p)]
if not cands:
    sys.exit(3)

cands.sort(key=lambda p: os.path.getmtime(p), reverse=True)
print(cands[0])
PY
}

# Only discover local prover if not using remote prover service
if [ -z "$PROVER_SERVICE_URL" ]; then
  if [ -z "${LIGERO_PROVER_BIN:-}" ] && [ -z "${LIGERO_PROVER_BINARY_PATH:-}" ]; then
    DISCOVERED_PROVER_BIN="$(discover_ligero_prover_bin || true)"
    if [ -n "$DISCOVERED_PROVER_BIN" ]; then
      export LIGERO_PROVER_BINARY_PATH="$DISCOVERED_PROVER_BIN"
    fi
  fi
else
  export PROVER_SERVICE_URL
fi

GENERATOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$GENERATOR_DIR/../.." && pwd)"

if [ -z "${CARGO_TARGET_DIR:-}" ]; then
  export CARGO_TARGET_DIR="$REPO_ROOT/target"
fi
BUILD_PROFILE="${BUILD_PROFILE:-debug}"
BIN_DIR="$CARGO_TARGET_DIR/$BUILD_PROFILE"
CARGO_BUILD_FLAGS=()
if [ "$BUILD_PROFILE" = "release" ]; then
  CARGO_BUILD_FLAGS+=(--release)
fi

# Determine a base nonce: prefer node-reported latest nonce + 1, fallback to local monotonic .last_nonce, then time
# Note: Chain generation numbers use milliseconds, so we use $(date +%s)000 as fallback
NONCE_STATE_FILE="$GENERATOR_DIR/.last_nonce"
BASE_NONCE=$(($(date +%s) * 1000))

# Try to fetch latest from node for this key
NODE_API_URL="${NODE_API_URL:-http://localhost:12346}"
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build "${CARGO_BUILD_FLAGS[@]}" --bin fetch-nonce 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"
LATEST_ON_NODE=$("$BIN_DIR/fetch-nonce" "$NODE_API_URL" "$PRIVATE_KEY_FILE" 2>/dev/null || echo "")

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

# Pool FVK public key for Level-B viewing (32 bytes hex, with or without 0x prefix)
# When set, FVKs are fetched from the FVK service and proofs include pool-signed viewer attestations.
# Example: POOL_FVK_PK=1ecf7f45dd35e4edc0e09205804211d753725bf7b13c54dd5f98f8e9bfec6abc
# Deprecated: AUTHORITY_FVK is still supported for backward compatibility but POOL_FVK_PK is preferred.
POOL_FVK_PK="${POOL_FVK_PK:-}"
# FVK service URL (default: http://127.0.0.1:8088)
MIDNIGHT_FVK_SERVICE_URL="${MIDNIGHT_FVK_SERVICE_URL:-http://127.0.0.1:8088}"

# Optional: fund the sender before shielded deposit (uses bank transfer via seq HTTP API)
FUNDER_KEY_FILE="${FUNDER_KEY_FILE:-$REPO_ROOT/examples/test-data/keys/token_deployer_private_key.json}"
FUND_AMOUNT="${FUND_AMOUNT:-1000}" # clear tokens to send to sender

# Endpoint - proof verifier service orchestrates sequencer submissions
VERIFIER_ENDPOINT="${VERIFIER_ENDPOINT:-http://localhost:8080/midnight-privacy}"

echo -e "${BLUE}=== Midnight Privacy: Deposit + Withdraw Flow ===${NC}\n"
echo "Parameters:"
echo "  Deposit: $DEPOSIT_AMOUNT"
echo "  Withdraw: $WITHDRAW_AMOUNT"
echo "  Change: $((DEPOSIT_AMOUNT - WITHDRAW_AMOUNT)) (stays shielded)"
echo "  Nonce: $NONCE"
echo "  Proof Verifier: $VERIFIER_ENDPOINT"
if [ -n "$PROVER_SERVICE_URL" ]; then
    echo "  Prover: $PROVER_SERVICE_URL (remote)"
else
    echo "  Prover: local binary"
fi
if [ -n "$POOL_FVK_PK" ]; then
    echo "  Pool FVK PK: ${POOL_FVK_PK:0:16}... (Level-B viewing via FVK service)"
    echo "  FVK Service: $MIDNIGHT_FVK_SERVICE_URL"
fi
echo ""

# Build generators if needed
echo -e "${YELLOW}Building generators...${NC}"
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build "${CARGO_BUILD_FLAGS[@]}" --bin midnight-deposit-generator --bin withdraw-generator 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"
echo ""

# Step 0: fund sender if requested (native Rust funder)
# Note: fund binary fetches its own generation from the node, no need to pass FUND_NONCE
if [ -n "$FUND_AMOUNT" ] && [ "$FUND_AMOUNT" != "0" ]; then
  echo -e "${YELLOW}Step 0: Funding sender with $FUND_AMOUNT tokens${NC}"
  cd "$GENERATOR_DIR"
  SKIP_GUEST_BUILD=1 cargo build "${CARGO_BUILD_FLAGS[@]}" --bin fund 2>&1 | grep -E "Compiling|Finished" || true
  cd "$REPO_ROOT"
  env \
    NODE_API_URL="${NODE_API_URL:-http://localhost:12346}" \
    RECIPIENT="$RECIPIENT" \
    FUND_AMOUNT="$FUND_AMOUNT" \
    FUNDER_KEY_FILE="$FUNDER_KEY_FILE" \
    "$BIN_DIR/fund"
  echo -e "${GREEN}✓ Funding submitted via native funder${NC}\n"
fi

# Step 1: Generate and send deposit
echo -e "${YELLOW}Step 1: Deposit${NC}"
export DEPOSIT_AMOUNT NONCE PRIVATE_KEY_FILE
cd "$GENERATOR_DIR"
"$BIN_DIR/midnight-deposit-generator" "midnight_deposit_tx.bin" > /tmp/deposit.log
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

echo -e "${GREEN}✓ Deposit sent${NC}"
echo "  Note position: $NOTE_POSITION"
if [ -n "$ANCHOR_ROOT" ] && [ "$ANCHOR_ROOT" != "null" ]; then
    echo "  Anchor root: $(echo $ANCHOR_ROOT | jq -r 'if type == "array" then (.[0:8] | map(tostring) | join(",")) else . end')"
else
    echo "  Anchor root: (not returned from notes endpoint)"
fi
echo ""

# Wait for confirmation
echo -e "${YELLOW}Waiting 3 seconds for confirmation...${NC}"
sleep 3
echo ""

# Step 2: Generate and send withdrawal
echo -e "${YELLOW}Step 2: Withdraw${NC}"

# Increment nonce for withdrawal (prefer node latest + 1)
LATEST_ON_NODE=$("$BIN_DIR/fetch-nonce" "${NODE_API_URL:-http://localhost:12346}" "$PRIVATE_KEY_FILE" 2>/dev/null || echo "")
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
NOTE_SPEND_SK=$(cat "$NOTE_DETAILS_FILE" | jq -r '.spend_sk')

# Build the withdrawal generator if needed and run it
cd "$GENERATOR_DIR"
SKIP_GUEST_BUILD=1 cargo build "${CARGO_BUILD_FLAGS[@]}" --bin withdraw-generator 2>&1 | grep -E "Compiling|Finished" || true
cd "$REPO_ROOT"

# Set environment and generate withdrawal using withdraw_generator.rs (same as transfer flow)
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-note_spend_guest}"
export LIGERO_PACKING="${LIGERO_PACKING:-8192}"
unset LIGERO_SHADER_PATH
# Export pool FVK configuration (for Level-B viewing support)
if [ -n "$POOL_FVK_PK" ]; then
    export POOL_FVK_PK
    export MIDNIGHT_FVK_SERVICE_URL
    echo "  Pool FVK PK: ${POOL_FVK_PK:0:16}... (Level-B viewing via FVK service)"
fi

# Map deposit note details into withdraw-generator inputs
export OUT1_DOMAIN="$NOTE_DOMAIN"
export OUT1_VALUE="$NOTE_VALUE"
export OUT1_RHO="$NOTE_RHO"
export OUT1_SPEND_SK="$NOTE_SPEND_SK"
export OUT1_POSITION="$NOTE_POSITION"
export TRANSFER_ROOT="$ANCHOR_ROOT"
export NONCE=$WITHDRAW_NONCE
export PRIVATE_KEY_FILE
export NODE_API_URL
export WITHDRAW_AMOUNT RECIPIENT

cd "$GENERATOR_DIR"
"$BIN_DIR/withdraw-generator" 2>&1 | tee /tmp/withdraw.log
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
