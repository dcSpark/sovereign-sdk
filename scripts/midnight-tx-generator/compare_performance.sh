#!/bin/bash

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
CYAN='\033[0;36m'
NC='\033[0m'

GENERATOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Function to get timestamp in milliseconds (cross-platform)
get_timestamp_ms() {
    if [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS: use python for millisecond precision
        python3 -c 'import time; print(int(time.time() * 1000))'
    else
        # Linux: use date with nanoseconds
        echo $(($(date +%s%N)/1000000))
    fi
}

echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}   Performance Comparison${NC}"
echo -e "${CYAN}   Verifier Service vs Direct Sequencer${NC}"
echo -e "${CYAN}========================================${NC}\n"

# Test 1: Direct to Sequencer
echo -e "${BLUE}[Test 1] Running DIRECT TO SEQUENCER flow...${NC}\n"
DIRECT_START=$(get_timestamp_ms)
DIRECT_SUCCESS=true
"$GENERATOR_DIR/deposit_and_withdraw_direct.sh" 2>&1 | tee /tmp/direct_test.log || DIRECT_SUCCESS=false
DIRECT_END=$(get_timestamp_ms)
DIRECT_DURATION=$((DIRECT_END - DIRECT_START))

echo ""
echo -e "${YELLOW}Waiting 5 seconds before next test...${NC}"
sleep 5
echo ""

# Test 2: Through Verifier Service
echo -e "${BLUE}[Test 2] Running THROUGH VERIFIER SERVICE flow...${NC}\n"
VERIFIER_START=$(get_timestamp_ms)
VERIFIER_SUCCESS=true
"$GENERATOR_DIR/deposit_and_withdraw.sh" 2>&1 | tee /tmp/verifier_test.log || VERIFIER_SUCCESS=false
VERIFIER_END=$(get_timestamp_ms)
VERIFIER_DURATION=$((VERIFIER_END - VERIFIER_START))

echo ""
echo ""
echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}   PERFORMANCE RESULTS${NC}"
echo -e "${CYAN}========================================${NC}\n"

echo -e "${GREEN}Direct to Sequencer:${NC}"
if [ "$DIRECT_SUCCESS" = true ]; then
    echo -e "  Status: ${GREEN}✓ Success${NC}"
else
    echo -e "  Status: ${RED}✗ Failed${NC}"
fi
echo -e "  Total Time: ${DIRECT_DURATION}ms"

echo ""
echo -e "${GREEN}Through Verifier Service:${NC}"
if [ "$VERIFIER_SUCCESS" = true ]; then
    echo -e "  Status: ${GREEN}✓ Success${NC}"
else
    echo -e "  Status: ${RED}✗ Failed${NC}"
fi
echo -e "  Total Time: ${VERIFIER_DURATION}ms"

echo ""
echo -e "${YELLOW}Analysis:${NC}"

if [ "$DIRECT_SUCCESS" = true ] && [ "$VERIFIER_SUCCESS" = true ]; then
    DIFF=$((VERIFIER_DURATION - DIRECT_DURATION))
    if [ $DIFF -gt 0 ]; then
        if [ $DIRECT_DURATION -gt 0 ]; then
            PERCENT=$(awk "BEGIN {printf \"%.2f\", ($DIFF / $DIRECT_DURATION) * 100}")
            echo -e "  Verifier Service adds: ${RED}+${DIFF}ms${NC} (${PERCENT}% overhead)"
        else
            echo -e "  Verifier Service adds: ${RED}+${DIFF}ms${NC}"
        fi
    else
        DIFF_ABS=$((DIRECT_DURATION - VERIFIER_DURATION))
        if [ $VERIFIER_DURATION -gt 0 ]; then
            PERCENT=$(awk "BEGIN {printf \"%.2f\", ($DIFF_ABS / $VERIFIER_DURATION) * 100}")
            echo -e "  Direct is slower by: ${RED}+${DIFF_ABS}ms${NC} (${PERCENT}%)"
        else
            echo -e "  Direct is slower by: ${RED}+${DIFF_ABS}ms${NC}"
        fi
    fi
elif [ "$VERIFIER_SUCCESS" = true ]; then
    echo -e "  ${YELLOW}Direct submission failed, only verifier service succeeded${NC}"
elif [ "$DIRECT_SUCCESS" = true ]; then
    echo -e "  ${YELLOW}Verifier service failed, only direct submission succeeded${NC}"
else
    echo -e "  ${RED}Both tests failed${NC}"
fi

echo ""
echo -e "${YELLOW}Detailed logs available at:${NC}"
echo -e "  Direct: /tmp/direct_test.log"
echo -e "  Verifier: /tmp/verifier_test.log"
echo ""

# Extract detailed metrics for deeper analysis
echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}   SEQUENCER PROCESSING BREAKDOWN${NC}"
echo -e "${CYAN}========================================${NC}\n"

# Extract worker metrics (from withdraw, as it has proof verification)
WORKER_NODE_SUBMIT=$(grep '"node_submit_ms":' /tmp/verifier_test.log 2>/dev/null | tail -1 | sed 's/.*: \([0-9.]*\).*/\1/' || echo "")
WORKER_PROOF_VERIFY=$(grep '"proof_verify_ms":' /tmp/verifier_test.log 2>/dev/null | tail -1 | sed 's/.*: \([0-9.]*\).*/\1/' || echo "")
WORKER_SIG_VERIFY=$(grep '"signature_verify_ms":' /tmp/verifier_test.log 2>/dev/null | tail -1 | sed 's/.*: \([0-9.]*\).*/\1/' || echo "")
WORKER_DESERIALIZE=$(grep '"deserialize_ms":' /tmp/verifier_test.log 2>/dev/null | tail -1 | sed 's/.*: \([0-9.]*\).*/\1/' || echo "")
WORKER_TX_CREATION=$(grep '"tx_creation_ms":' /tmp/verifier_test.log 2>/dev/null | tail -1 | sed 's/.*: \([0-9.]*\).*/\1/' || echo "")

# Extract direct sequencer timings
if [ -f "$GENERATOR_DIR/.direct_timings" ]; then
    source "$GENERATOR_DIR/.direct_timings"
else
    DEPOSIT_SUBMIT_MS=""
    WITHDRAW_SUBMIT_MS=""
fi

if [ "$VERIFIER_SUCCESS" = true ] && [ -n "$WORKER_NODE_SUBMIT" ]; then
    echo -e "${GREEN}Worker Service Path (Pre-authenticated):${NC}"
    echo -e "  ├─ Proof verification: ${WORKER_PROOF_VERIFY}ms ${YELLOW}(parallel, doesn't block sequencer)${NC}"
    echo -e "  ├─ Signature verification: ${WORKER_SIG_VERIFY}ms ${YELLOW}(done by worker)${NC}"
    echo -e "  ├─ Transaction creation: ${WORKER_TX_CREATION}ms"
    echo -e "  └─ Sequencer processing: ${GREEN}${WORKER_NODE_SUBMIT}ms${NC} ${YELLOW}(skips verification!)${NC}"
    echo ""
fi

if [ "$DIRECT_SUCCESS" = true ]; then
    echo -e "${GREEN}Direct Sequencer Path (Full verification):${NC}"
    if [ -n "$DEPOSIT_SUBMIT_MS" ] && [ -n "$WITHDRAW_SUBMIT_MS" ]; then
        echo -e "  ├─ Deposit sequencer time: ${DEPOSIT_SUBMIT_MS}ms ${YELLOW}(includes signature verification)${NC}"
        echo -e "  └─ Withdraw sequencer time: ${WITHDRAW_SUBMIT_MS}ms ${YELLOW}(includes signature verification)${NC}"
    else
        echo -e "  └─ Total time: ${DIRECT_DURATION}ms ${YELLOW}(includes signature verification)${NC}"
    fi
    echo ""
fi

# Calculate the performance benefit (compare withdraw times as they include proof verification)
if [ "$DIRECT_SUCCESS" = true ] && [ "$VERIFIER_SUCCESS" = true ] && [ -n "$WORKER_NODE_SUBMIT" ] && [ -n "$WITHDRAW_SUBMIT_MS" ]; then
    WORKER_NODE_SUBMIT_INT=$(printf "%.0f" "$WORKER_NODE_SUBMIT")
    
    # Compare withdraw sequencer times (apples-to-apples)
    SEQUENCER_SPEEDUP=$((WITHDRAW_SUBMIT_MS - WORKER_NODE_SUBMIT_INT))
    
    if [ $SEQUENCER_SPEEDUP -gt 0 ] && [ $WITHDRAW_SUBMIT_MS -gt 0 ]; then
        SPEEDUP_PERCENT=$(awk "BEGIN {printf \"%.1f\", ($SEQUENCER_SPEEDUP / $WITHDRAW_SUBMIT_MS) * 100}")
        
        echo -e "${CYAN}========================================${NC}"
        echo -e "${CYAN}   OPTIMIZATION BENEFITS${NC}"
        echo -e "${CYAN}========================================${NC}\n"
        
        echo -e "${GREEN}✓ Sequencer Speedup (Withdraw Transaction):${NC}"
        echo -e "  Worker path is ${GREEN}${SEQUENCER_SPEEDUP}ms faster${NC} (${SPEEDUP_PERCENT}% improvement)"
        echo -e "  Direct: ${WITHDRAW_SUBMIT_MS}ms → Worker: ${WORKER_NODE_SUBMIT_INT}ms"
        echo -e "  ${YELLOW}This is pure sequencer processing time (apples-to-apples)${NC}"
        echo ""
        
        echo -e "${GREEN}✓ What happens to the saved time?${NC}"
        if [ -n "$WORKER_PROOF_VERIFY" ]; then
            PROOF_INT=$(printf "%.0f" "$WORKER_PROOF_VERIFY")
            echo -e "  Proof verification: ${WORKER_PROOF_VERIFY}ms ${YELLOW}(moved to parallel workers!)${NC}"
        fi
        if [ -n "$WORKER_SIG_VERIFY" ]; then
            echo -e "  Signature verification: ${WORKER_SIG_VERIFY}ms ${YELLOW}(moved to workers!)${NC}"
        fi
        echo -e "  This work is OFF the critical path of sequencer processing"
        echo ""
        
        echo -e "${GREEN}✓ Key Advantages:${NC}"
        echo -e "  • Workers can run in parallel (horizontal scaling)"
        echo -e "  • Sequencer is freed from expensive proof verification"
        echo -e "  • Pre-authentication skips redundant signature checks"
        echo -e "  • Lightweight transactions (proof stripped, ~3MB saved)"
        echo ""
        
        # Show throughput implications
        if [ $WORKER_NODE_SUBMIT_INT -gt 0 ]; then
            DIRECT_TPS=$(awk "BEGIN {printf \"%.1f\", 1000 / $WITHDRAW_SUBMIT_MS}")
            WORKER_TPS=$(awk "BEGIN {printf \"%.1f\", 1000 / $WORKER_NODE_SUBMIT_INT}")
            TPS_IMPROVEMENT=$(awk "BEGIN {printf \"%.1f\", ($WORKER_TPS - $DIRECT_TPS) / $DIRECT_TPS * 100}")
            
            echo -e "${GREEN}✓ Throughput Impact (Sequential Processing):${NC}"
            echo -e "  Direct path: ~${DIRECT_TPS} tx/sec"
            echo -e "  Worker path: ~${WORKER_TPS} tx/sec"
            echo -e "  ${GREEN}${TPS_IMPROVEMENT}% throughput increase${NC}"
            echo ""
        fi
    fi
fi

# Show detailed metrics
echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}   DETAILED WORKER METRICS${NC}"
echo -e "${CYAN}========================================${NC}\n"

VERIFIER_METRICS=$(grep -A 10 '"metrics":' /tmp/verifier_test.log 2>/dev/null | tail -11 || echo "")
if [ -n "$VERIFIER_METRICS" ]; then
    echo "$VERIFIER_METRICS"
else
    echo "  No detailed metrics found"
fi

