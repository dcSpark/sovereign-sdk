#!/bin/bash
# TPS Test Script with Proof Verifier Service
# 
# This script pre-signs transactions and submits them to the proof verifier service
# which verifies them in parallel and forwards non-ZK TXs to the node.
#
# Usage:
#   ./test_tps_with_verifier.sh [num_transactions] [value_start] [concurrent_jobs]
#
# Examples:
#   ./test_tps_with_verifier.sh 50 1 10
#   # Generates 50 proofs (values 1-50), submits with 10 workers to verifier service

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# Default values
NUM_TRANSACTIONS=${1:-50}
VALUE_START=${2:-1}
CONCURRENT_JOBS=${3:-10}
MAX_FEE="100000000000"
VERIFIER_URL="http://127.0.0.1:8080"  # Verifier service, not node!
NODE_RPC_URL="http://127.0.0.1:12346"   # For reference/health checks
ACCOUNT_NICKNAME="DANGER__DO_NOT_USE_WITH_REAL_MONEY"

# Paths
LIGERO_ADAPTER_DIR="../../crates/adapters/ligero"
SOV_CLI="../../target/debug/sov-cli"
TEMP_DIR="./tps_test_verifier_temp"

echo -e "${CYAN}╔═══════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   TPS Test with Parallel Proof Verifier Service      ║${NC}"
echo -e "${CYAN}╚═══════════════════════════════════════════════════════╝${NC}"
echo ""

# Validate value range
VALUE_END=$((VALUE_START + NUM_TRANSACTIONS - 1))
if [ "$VALUE_END" -gt 100 ]; then
    echo -e "${RED}Error: Value range exceeds valid range [0, 100]${NC}"
    echo -e "  Starting value: ${VALUE_START}"
    echo -e "  Ending value: ${VALUE_END}"
    exit 1
fi

echo -e "${YELLOW}Configuration:${NC}"
echo -e "  • Number of transactions: ${GREEN}${NUM_TRANSACTIONS}${NC}"
echo -e "  • Value range:            ${GREEN}${VALUE_START}-${VALUE_END}${NC}"
echo -e "  • Concurrent workers:     ${GREEN}${CONCURRENT_JOBS}${NC}"
echo -e "  • Verifier URL:           ${GREEN}${VERIFIER_URL}${NC}"
echo -e "  • Node RPC URL:           ${GREEN}${NODE_RPC_URL}${NC}"
echo ""

# Check if verifier service is running
echo -e "${BLUE}Checking verifier service health...${NC}"
if ! curl -s "${VERIFIER_URL}/health" > /dev/null 2>&1; then
    echo -e "${RED}Error: Verifier service not reachable at ${VERIFIER_URL}${NC}"
    echo ""
    echo -e "Please start the verifier service first:"
    echo -e "  cd crates/utils/sov-proof-verifier-service"
    echo -e "  cargo run --release"
    exit 1
fi
echo -e "${GREEN}✓ Verifier service is healthy${NC}"
echo ""

# Check if sov-cli exists
if [ ! -f "$SOV_CLI" ]; then
    echo -e "${RED}Error: sov-cli not found at ${SOV_CLI}${NC}"
    echo "Please build the project first with: make build"
    exit 1
fi

# Create temp directory
rm -rf "$TEMP_DIR"
mkdir -p "$TEMP_DIR"

echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo -e "${YELLOW}Phase 1: Generating ${NUM_TRANSACTIONS} proofs${NC}"
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo ""

PROOF_GEN_START=$(date +%s.%N)

for i in $(seq 0 $((NUM_TRANSACTIONS - 1))); do
    VALUE=$((VALUE_START + i))
    echo -e "${BLUE}[$(($i + 1))/${NUM_TRANSACTIONS}]${NC} Generating proof for value: ${GREEN}${VALUE}${NC}"
    
    (cd "$LIGERO_ADAPTER_DIR" && cargo run --example generate_value_proof --features native -- "$VALUE" > /dev/null 2>&1)
    cp "$LIGERO_ADAPTER_DIR/value_tx.json" "$TEMP_DIR/value_tx_${i}.json"
done

PROOF_GEN_END=$(date +%s.%N)
PROOF_GEN_TIME=$(echo "$PROOF_GEN_END - $PROOF_GEN_START" | bc)

echo ""
echo -e "${GREEN}✓ Proof generation complete!${NC}"
echo -e "  Time taken: ${GREEN}${PROOF_GEN_TIME}s${NC}"
echo -e "  Avg per proof: ${GREEN}$(echo "scale=3; $PROOF_GEN_TIME / $NUM_TRANSACTIONS" | bc)s${NC}"
echo ""

echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo -e "${YELLOW}Phase 2: Pre-signing ${NUM_TRANSACTIONS} transactions${NC}"
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo ""

SIGN_START=$(date +%s.%N)

# Get current nonce
echo -e "${BLUE}Fetching current nonce from sequencer...${NC}"
CURRENT_NONCE=$("$SOV_CLI" rpc account-nonce by-nickname "$ACCOUNT_NICKNAME" 2>/dev/null || echo "0")
echo -e "  Current nonce: ${GREEN}${CURRENT_NONCE}${NC}"
echo ""

# Sign all transactions
for i in $(seq 0 $((NUM_TRANSACTIONS - 1))); do
    NONCE=$((CURRENT_NONCE + i))
    echo -e "${BLUE}[$(($i + 1))/${NUM_TRANSACTIONS}]${NC} Signing transaction ${i} with nonce ${GREEN}${NONCE}${NC}"
    
    SIGNED_HEX=$("$SOV_CLI" transactions sign \
        --generation "$NONCE" \
        --key-nickname "$ACCOUNT_NICKNAME" \
        --json-output \
        from-file value-setter-zk \
        --max-fee "$MAX_FEE" \
        --path "$TEMP_DIR/value_tx_${i}.json" \
        2>/dev/null | jq -r '.signed_tx')
    
    if [ -z "$SIGNED_HEX" ] || [ "$SIGNED_HEX" = "null" ]; then
        echo -e "${RED}Error: Failed to sign transaction ${i}${NC}"
        exit 1
    fi
    
    "$SOV_CLI" transactions clean > /dev/null 2>&1
    
    SIGNED_HEX=${SIGNED_HEX#0x}
    echo "$SIGNED_HEX" | xxd -r -p | base64 > "$TEMP_DIR/signed_tx_${i}.b64"
    
    # Create JSON payload for verifier service
    cat > "$TEMP_DIR/verifier_payload_${i}.json" <<EOF
{
  "body": "$(cat "$TEMP_DIR/signed_tx_${i}.b64")"
}
EOF
done

SIGN_END=$(date +%s.%N)
SIGN_TIME=$(echo "$SIGN_END - $SIGN_START" | bc)

echo ""
echo -e "${GREEN}✓ Pre-signing complete!${NC}"
echo -e "  Time taken: ${GREEN}${SIGN_TIME}s${NC}"
echo -e "  Avg per signature: ${GREEN}$(echo "scale=3; $SIGN_TIME / $NUM_TRANSACTIONS" | bc)s${NC}"
echo ""

# Function to submit to verifier service
submit_to_verifier() {
    local idx=$1
    local temp_dir=$2
    local verifier_url=$3
    
    local payload_file="$temp_dir/verifier_payload_${idx}.json"
    local result_file="$temp_dir/verify_result_${idx}.json"
    
    # Submit to verifier service (not node!)
    HTTP_CODE=$(curl -s -o "$result_file" -w "%{http_code}" \
        -X POST "${verifier_url}/verify-and-submit" \
        -H "Content-Type: application/json" \
        --data @"$payload_file" \
        --max-time 30)
    
    if [ "$HTTP_CODE" -eq 200 ]; then
        # Extract metrics from response
        VERIFY_TIME=$(jq -r '.metrics.proof_verify_ms // 0' "$result_file" 2>/dev/null)
        TOTAL_TIME=$(jq -r '.metrics.total_ms // 0' "$result_file" 2>/dev/null)
        TX_HASH=$(jq -r '.tx_hash // "unknown"' "$result_file" 2>/dev/null)
        echo "SUCCESS:$idx:$VERIFY_TIME:$TOTAL_TIME:$TX_HASH"
    else
        echo "FAILED:$idx:$HTTP_CODE"
        echo "=== Transaction $idx (HTTP $HTTP_CODE) ===" >> "$temp_dir/errors.log" 2>/dev/null || true
        cat "$result_file" >> "$temp_dir/errors.log" 2>/dev/null || true
        echo "" >> "$temp_dir/errors.log" 2>/dev/null || true
    fi
}

export -f submit_to_verifier
export TEMP_DIR
export VERIFIER_URL

echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo -e "${YELLOW}Phase 3: Parallel Verification & Submission${NC}"
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}Submitting to verifier service (${CONCURRENT_JOBS} workers)${NC}"
echo -e "${CYAN}Verifier will verify proofs in parallel!${NC}"
echo ""

SUBMIT_START=$(date +%s.%N)

if command -v parallel &> /dev/null; then
    RESULTS=$(seq 0 $((NUM_TRANSACTIONS - 1)) | \
        parallel --will-cite -j "$CONCURRENT_JOBS" --line-buffer \
        submit_to_verifier {} "$TEMP_DIR" "$VERIFIER_URL")
else
    RESULTS=$(seq 0 $((NUM_TRANSACTIONS - 1)) | \
        xargs -P "$CONCURRENT_JOBS" -I {} bash -c "submit_to_verifier {} \"$TEMP_DIR\" \"$VERIFIER_URL\"")
fi

SUBMIT_END=$(date +%s.%N)
SUBMIT_TIME=$(echo "$SUBMIT_END - $SUBMIT_START" | bc)

echo ""
echo -e "${GREEN}✓ Verification & submission complete!${NC}"

# Parse results
SUCCESSFUL_TXS=$(echo "$RESULTS" | grep -c "SUCCESS:" 2>/dev/null || echo "0")
FAILED_TXS=$(echo "$RESULTS" | grep -c "FAILED:" 2>/dev/null || echo "0")

# Calculate average verification time
TOTAL_VERIFY_TIME=0
VERIFY_COUNT=0
while IFS=':' read -r status idx verify_ms total_ms hash; do
    if [ "$status" = "SUCCESS" ] && [ -n "$verify_ms" ]; then
        TOTAL_VERIFY_TIME=$(echo "$TOTAL_VERIFY_TIME + $verify_ms" | bc)
        VERIFY_COUNT=$((VERIFY_COUNT + 1))
    fi
done <<< "$RESULTS"

if [ "$VERIFY_COUNT" -gt 0 ]; then
    AVG_VERIFY_TIME=$(echo "scale=2; $TOTAL_VERIFY_TIME / $VERIFY_COUNT" | bc)
else
    AVG_VERIFY_TIME=0
fi

echo -e "  Time taken: ${GREEN}${SUBMIT_TIME}s${NC}"
echo -e "  Successful: ${GREEN}${SUCCESSFUL_TXS}${NC}"
echo -e "  Failed: ${RED}${FAILED_TXS}${NC}"
echo -e "  Avg proof verification time: ${CYAN}${AVG_VERIFY_TIME}ms${NC}"
echo ""

# Calculate total metrics
TOTAL_TIME=$(echo "$PROOF_GEN_TIME + $SIGN_TIME + $SUBMIT_TIME" | bc)

echo -e "${CYAN}╔═══════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║     TPS Test with Verifier Service Results           ║${NC}"
echo -e "${CYAN}╚═══════════════════════════════════════════════════════╝${NC}"
echo -e "${YELLOW}Timing Breakdown:${NC}"
echo -e "  • Proof generation:      ${GREEN}${PROOF_GEN_TIME}s${NC}"
echo -e "  • Pre-signing:           ${GREEN}${SIGN_TIME}s${NC}"
echo -e "  • Parallel verification: ${GREEN}${SUBMIT_TIME}s${NC}"
echo -e "  • Total time:            ${GREEN}${TOTAL_TIME}s${NC}"

echo -e "${YELLOW}Verification Metrics:${NC}"
echo -e "  • Transactions attempted: ${GREEN}${NUM_TRANSACTIONS}${NC}"
echo -e "  • Transactions successful: ${GREEN}${SUCCESSFUL_TXS}${NC}"
echo -e "  • Transactions failed:    ${RED}${FAILED_TXS}${NC}"
echo -e "  • Avg proof verify time:  ${CYAN}${AVG_VERIFY_TIME}ms${NC}"

if [ "$NUM_TRANSACTIONS" -gt 0 ]; then
    SUCCESS_RATE=$(echo "scale=1; 100 * $SUCCESSFUL_TXS / $NUM_TRANSACTIONS" | bc)
    echo -e "  • Success rate:          ${GREEN}${SUCCESS_RATE}%${NC}"
fi

if [ "$SUBMIT_TIME" != "0" ] && [ "$SUCCESSFUL_TXS" -gt 0 ]; then
    VERIFIER_TPS=$(echo "scale=2; $SUCCESSFUL_TXS / $SUBMIT_TIME" | bc)
    OVERALL_TPS=$(echo "scale=2; $SUCCESSFUL_TXS / $TOTAL_TIME" | bc)
    echo -e "  • Verifier TPS:          ${GREEN}${VERIFIER_TPS}${NC} tx/s"
    echo -e "  • Overall TPS:           ${GREEN}${OVERALL_TPS}${NC} tx/s"
fi

echo ""
echo -e "${GREEN}✓ Test complete!${NC}"
echo ""

if [ "$SUCCESSFUL_TXS" -eq "$NUM_TRANSACTIONS" ]; then
    echo -e "${CYAN}🎉 Perfect Success!${NC}"
    echo -e "  All ${NUM_TRANSACTIONS} transactions verified and submitted in ${GREEN}${SUBMIT_TIME}s${NC}"
    echo -e "  Parallel verification achieved ${GREEN}${VERIFIER_TPS} tx/s${NC}"
    echo -e "  This is a ${GREEN}$(echo "scale=1; $VERIFIER_TPS / 1.58" | bc)x speedup${NC} vs direct node submission!"
elif [ "$FAILED_TXS" -gt 0 ]; then
    echo -e "${YELLOW}⚠️  Some transactions failed${NC}"
    echo -e "  Check errors at: ${TEMP_DIR}/errors.log"
fi

