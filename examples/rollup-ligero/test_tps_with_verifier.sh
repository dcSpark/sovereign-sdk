#!/bin/bash
# TPS Test Script with Proof Verifier Service (Fully Parallelized)
# 
# This script parallelizes ALL stages:
#   1. Proof generation (parallel)
#   2. Transaction signing (parallel)
#   3. Verification & submission (parallel via verifier service)
#
# The verifier service verifies ZK proofs in parallel and forwards non-ZK TXs to the node.
#
# Usage:
#   ./test_tps_with_verifier.sh [num_transactions] [value_start] [concurrent_jobs] [--reuse]
#
# Examples:
#   ./test_tps_with_verifier.sh 50 1 10
#   # Generates 50 proofs (values 1-50) using 10 parallel workers for all stages
#
#   ./test_tps_with_verifier.sh 100 1 20 --reuse
#   # Reuses existing proofs and signatures, only runs verification phase

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# Parse arguments
REUSE_MODE=false
for arg in "$@"; do
    if [ "$arg" = "--reuse" ]; then
        REUSE_MODE=true
    fi
done

# Default values
NUM_TRANSACTIONS=${1:-50}
VALUE_START=${2:-1}
CONCURRENT_JOBS=${3:-50}
MAX_FEE="100000000000"
# VERIFIER_URL="https://ligero-verifier-302883622007.us-central1.run.app"
# NODE_RPC_URL="https://sovereign-test.shinkai.com"
VERIFIER_URL="http://127.0.0.1:8080"
NODE_RPC_URL="http://127.0.0.1:12346"
ACCOUNT_NICKNAME="DANGER__DO_NOT_USE_WITH_REAL_MONEY"

# Paths (use absolute paths to avoid issues when cd'ing into worker directories)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LIGERO_ADAPTER_DIR="$SCRIPT_DIR/../../crates/adapters/ligero"
SOV_CLI="$SCRIPT_DIR/../../target/debug/sov-cli"
TEMP_DIR="$SCRIPT_DIR/tps_test_verifier_temp"

echo -e "${CYAN}╔═══════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   TPS Test with Parallel Proof Verifier Service      ║${NC}"
echo -e "${CYAN}╚═══════════════════════════════════════════════════════╝${NC}"
echo ""

# Validate value range  
VALUE_END=$((VALUE_START + NUM_TRANSACTIONS - 1))
if [ "$VALUE_END" -gt 65535 ]; then
    echo -e "${RED}Error: Value range exceeds valid range [0, 65535]${NC}"
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
if [ "$REUSE_MODE" = true ]; then
    echo -e "  • Mode:                   ${CYAN}REUSE (skip gen/sign)${NC}"
else
    echo -e "  • Mode:                   ${CYAN}FULL (gen+sign+verify)${NC}"
fi
echo ""

# Check if verifier service is running
echo -e "${BLUE}Checking verifier service health...${NC}"
if ! curl -s "${VERIFIER_URL}/health" > /dev/null 2>&1; then
    echo -e "${RED}Error: Verifier service not reachable at ${VERIFIER_URL}${NC}"
    echo ""
    echo -e "Please start the verifier service first:"
    echo -e "  ./run_verifier_service.sh"
    echo -e "  (or run in background: ./run_verifier_service.sh &)"
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
if [ "$REUSE_MODE" = false ]; then
    rm -rf "$TEMP_DIR"
fi
mkdir -p "$TEMP_DIR"

# Validate reuse mode
if [ "$REUSE_MODE" = true ]; then
    # Check if required files exist
    MISSING_FILES=0
    for i in $(seq 0 $((NUM_TRANSACTIONS - 1))); do
        if [ ! -f "$TEMP_DIR/verifier_payload_${i}.json" ]; then
            MISSING_FILES=$((MISSING_FILES + 1))
        fi
    done
    
    if [ "$MISSING_FILES" -gt 0 ]; then
        echo -e "${RED}Error: --reuse mode requires existing files, but $MISSING_FILES are missing${NC}"
        echo -e "${YELLOW}Hint: Run without --reuse first to generate proofs and signatures${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}✓ Found all $NUM_TRANSACTIONS pre-signed transactions${NC}"
    echo -e "${CYAN}  Skipping proof generation and signing phases${NC}"
    echo ""
    
    # Set times to 0 for reuse mode
    PROOF_GEN_TIME=0
    SIGN_TIME=0
fi

if [ "$REUSE_MODE" = false ]; then
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo -e "${YELLOW}Phase 1: Generating ${NUM_TRANSACTIONS} proofs (parallel)${NC}"
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo ""

# Function to generate a single proof
generate_proof() {
    local idx=$1
    local value=$2
    local ligero_dir=$3
    local temp_dir=$4
    
    # CRITICAL FIX: Avoid race condition by using worker-specific directory
    # Previous bug: All 20 workers wrote to the same ligero_dir/value_tx.json
    # This caused:
    #   - Overwrites: tx 0 would get value 14's proof (worker 14 overwrote it)
    #   - Truncation: tx 46-49 got "unexpected EOF" (copied while being written)
    
    local worker_dir="$temp_dir/worker_${idx}"
    mkdir -p "$worker_dir"
    
    # Run proof generation in worker-specific directory (writes to worker_dir/value_tx.json)
    (cd "$worker_dir" && cargo run --manifest-path "$ligero_dir/Cargo.toml" \
        --example generate_value_proof --features native -- "$value" > /dev/null 2>&1)
    
    if [ -f "$worker_dir/value_tx.json" ]; then
        mv "$worker_dir/value_tx.json" "$temp_dir/value_tx_${idx}.json"
        rm -rf "$worker_dir"
        echo "PROOF_DONE:$idx:$value"
    else
        echo "PROOF_FAILED:$idx"
        return 1
    fi
}

export -f generate_proof
export LIGERO_ADAPTER_DIR
export TEMP_DIR
export VALUE_START

PROOF_GEN_START=$(date +%s.%N)

if command -v parallel &> /dev/null; then
    echo -e "${CYAN}Using GNU parallel with ${CONCURRENT_JOBS} workers${NC}"
    seq 0 $((NUM_TRANSACTIONS - 1)) | \
        parallel --will-cite -j "$CONCURRENT_JOBS" --line-buffer \
        'generate_proof {} $((VALUE_START + {})) "$LIGERO_ADAPTER_DIR" "$TEMP_DIR"'
else
    echo -e "${CYAN}Using xargs with ${CONCURRENT_JOBS} workers${NC}"
    seq 0 $((NUM_TRANSACTIONS - 1)) | \
        xargs -P "$CONCURRENT_JOBS" -I {} bash -c 'generate_proof {} $((VALUE_START + {})) "$LIGERO_ADAPTER_DIR" "$TEMP_DIR"'
fi

PROOF_GEN_END=$(date +%s.%N)
PROOF_GEN_TIME=$(echo "$PROOF_GEN_END - $PROOF_GEN_START" | bc)

echo ""
echo -e "${GREEN}✓ Proof generation complete!${NC}"
echo -e "  Time taken: ${GREEN}${PROOF_GEN_TIME}s${NC}"
echo -e "  Avg per proof: ${GREEN}$(echo "scale=3; $PROOF_GEN_TIME / $NUM_TRANSACTIONS" | bc)s${NC}"
echo ""
fi  # End of REUSE_MODE check for Phase 1

if [ "$REUSE_MODE" = false ]; then
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo -e "${YELLOW}Phase 2: Pre-signing ${NUM_TRANSACTIONS} transactions (parallel)${NC}"
echo -e "${YELLOW}════════════════════════════════════════════════════${NC}"
echo ""

# Get current nonce
echo -e "${BLUE}Fetching current nonce from sequencer...${NC}"
CURRENT_NONCE=$("$SOV_CLI" rpc account-nonce by-nickname "$ACCOUNT_NICKNAME" 2>/dev/null || echo "0")
echo -e "  Current nonce: ${GREEN}${CURRENT_NONCE}${NC}"
echo ""

# Function to sign a single transaction
sign_transaction() {
    local idx=$1
    local nonce=$2
    local sov_cli=$3
    local account_nickname=$4
    local max_fee=$5
    local temp_dir=$6
    
    SIGNED_HEX=$("$sov_cli" transactions sign \
        --generation "$nonce" \
        --key-nickname "$account_nickname" \
        --json-output \
        from-file value-setter-zk \
        --max-fee "$max_fee" \
        --path "$temp_dir/value_tx_${idx}.json" \
        2>/dev/null | jq -r '.signed_tx')
    
    if [ -z "$SIGNED_HEX" ] || [ "$SIGNED_HEX" = "null" ]; then
        echo "SIGN_FAILED:$idx"
        return 1
    fi
    
    "$sov_cli" transactions clean > /dev/null 2>&1
    
    SIGNED_HEX=${SIGNED_HEX#0x}
    echo "$SIGNED_HEX" | xxd -r -p | base64 > "$temp_dir/signed_tx_${idx}.b64"
    
    # Create JSON payload for verifier service
    cat > "$temp_dir/verifier_payload_${idx}.json" <<EOF
{
  "body": "$(cat "$temp_dir/signed_tx_${idx}.b64")"
}
EOF
    
    echo "SIGN_DONE:$idx:$nonce"
}

export -f sign_transaction
export SOV_CLI
export ACCOUNT_NICKNAME
export MAX_FEE
export CURRENT_NONCE

SIGN_START=$(date +%s.%N)

echo -e "${CYAN}Using ${CONCURRENT_JOBS} parallel workers for signing${NC}"
echo ""

if command -v parallel &> /dev/null; then
    SIGN_RESULTS=$(seq 0 $((NUM_TRANSACTIONS - 1)) | \
        parallel --will-cite -j "$CONCURRENT_JOBS" --line-buffer \
        'sign_transaction {} $((CURRENT_NONCE + {})) "$SOV_CLI" "$ACCOUNT_NICKNAME" "$MAX_FEE" "$TEMP_DIR"')
else
    SIGN_RESULTS=$(seq 0 $((NUM_TRANSACTIONS - 1)) | \
        xargs -P "$CONCURRENT_JOBS" -I {} bash -c 'sign_transaction {} $((CURRENT_NONCE + {})) "$SOV_CLI" "$ACCOUNT_NICKNAME" "$MAX_FEE" "$TEMP_DIR"')
fi

# Check for signing failures
SIGN_FAILURES=$(echo "$SIGN_RESULTS" | grep "SIGN_FAILED:" | wc -l | tr -d ' ')
if [ -z "$SIGN_FAILURES" ]; then
    SIGN_FAILURES=0
fi
if [ "$SIGN_FAILURES" -gt 0 ]; then
    echo -e "${RED}Error: $SIGN_FAILURES transaction(s) failed to sign${NC}"
    exit 1
fi

SIGN_END=$(date +%s.%N)
SIGN_TIME=$(echo "$SIGN_END - $SIGN_START" | bc)

echo ""
echo -e "${GREEN}✓ Pre-signing complete!${NC}"
echo -e "  Time taken: ${GREEN}${SIGN_TIME}s${NC}"
echo -e "  Avg per signature: ${GREEN}$(echo "scale=3; $SIGN_TIME / $NUM_TRANSACTIONS" | bc)s${NC}"
echo ""
fi  # End of REUSE_MODE check for Phase 2

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
SUCCESSFUL_TXS=$(echo "$RESULTS" | grep "SUCCESS:" | wc -l | tr -d ' ')
FAILED_TXS=$(echo "$RESULTS" | grep "FAILED:" | wc -l | tr -d ' ')
if [ -z "$SUCCESSFUL_TXS" ]; then SUCCESSFUL_TXS=0; fi
if [ -z "$FAILED_TXS" ]; then FAILED_TXS=0; fi

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
if [ "$REUSE_MODE" = true ]; then
    echo -e "  • Proof generation:      ${CYAN}SKIPPED (reuse mode)${NC}"
    echo -e "  • Pre-signing:           ${CYAN}SKIPPED (reuse mode)${NC}"
else
    echo -e "  • Proof generation:      ${GREEN}${PROOF_GEN_TIME}s${NC}"
    echo -e "  • Pre-signing:           ${GREEN}${SIGN_TIME}s${NC}"
fi
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
    if [ "$REUSE_MODE" = true ]; then
        echo -e "  All ${NUM_TRANSACTIONS} transactions verified & submitted in ${GREEN}${SUBMIT_TIME}s${NC}"
        echo -e "  ${CYAN}(Reused pre-generated proofs and signatures)${NC}"
    else
        echo -e "  All ${NUM_TRANSACTIONS} transactions completed in ${GREEN}${TOTAL_TIME}s${NC}"
    fi
    echo ""
    echo -e "${CYAN}Performance Breakdown:${NC}"
    if [ "$REUSE_MODE" = true ]; then
        echo -e "  • Parallel verification:     ${GREEN}${SUBMIT_TIME}s${NC} (${CONCURRENT_JOBS} workers)"
        echo -e "  • Pure verification TPS:     ${GREEN}${VERIFIER_TPS} tx/s${NC}"
    else
        echo -e "  • Parallel proof generation: ${GREEN}${PROOF_GEN_TIME}s${NC} (${CONCURRENT_JOBS} workers)"
        echo -e "  • Parallel pre-signing:      ${GREEN}${SIGN_TIME}s${NC} (${CONCURRENT_JOBS} workers)"
        echo -e "  • Parallel verification:     ${GREEN}${SUBMIT_TIME}s${NC} (${CONCURRENT_JOBS} workers)"
        echo ""
        echo -e "  Verifier TPS: ${GREEN}${VERIFIER_TPS} tx/s${NC}"
        echo -e "  Overall TPS: ${GREEN}${OVERALL_TPS} tx/s${NC}"
    fi
elif [ "$FAILED_TXS" -gt 0 ]; then
    echo -e "${YELLOW}⚠️  Some transactions failed${NC}"
    echo -e "  Check errors at: ${TEMP_DIR}/errors.log"
fi

