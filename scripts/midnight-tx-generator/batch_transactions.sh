#!/bin/bash
set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Default parameters
ITERATIONS="${1:-5}"  # Number of times to run (default: 5)
DELAY="${DELAY:-3}"   # Delay between iterations in seconds (default: 3)
DEPOSIT_AMOUNT="${DEPOSIT_AMOUNT:-1000}"
TRANSFER_OUT1="${TRANSFER_OUT1:-600}"
TRANSFER_OUT2="${TRANSFER_OUT2:-400}"
WITHDRAW_AMOUNT="${WITHDRAW_AMOUNT:-200}"

echo -e "${CYAN}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Midnight Privacy: Batch Transaction Generator               ║${NC}"
echo -e "${CYAN}╔════════════════════════════════════════════════════════════════╗${NC}"
echo ""
echo -e "${BLUE}Configuration:${NC}"
echo "  Iterations: $ITERATIONS"
echo "  Delay between iterations: ${DELAY}s"
echo "  Deposit amount: $DEPOSIT_AMOUNT"
echo "  Transfer split: $TRANSFER_OUT1 + $TRANSFER_OUT2"
echo "  Withdraw amount: $WITHDRAW_AMOUNT"
echo ""

# Track statistics
SUCCESSFUL_DEPOSITS=0
SUCCESSFUL_TRANSFERS=0
SUCCESSFUL_WITHDRAWALS=0
FAILED_ITERATIONS=0

START_TIME=$(date +%s)

# Function to get current stats
get_stats() {
    curl -s http://localhost:12346/modules/midnight-privacy/stats 2>/dev/null || echo '{}'
}

# Show initial stats
echo -e "${YELLOW}Initial Pool Statistics:${NC}"
INITIAL_STATS=$(get_stats)
echo "$INITIAL_STATS" | jq -C '.' 2>/dev/null || echo "$INITIAL_STATS"
echo ""

# Main loop
for i in $(seq 1 $ITERATIONS); do
    echo -e "${CYAN}════════════════════════════════════════════════════════════════${NC}"
    echo -e "${CYAN}  Iteration $i of $ITERATIONS${NC}"
    echo -e "${CYAN}════════════════════════════════════════════════════════════════${NC}"
    
    # Use timestamp-based nonce to ensure uniqueness
    export NONCE=$(date +%s%N | cut -b1-13)
    
    # Run the deposit/transfer/withdraw flow
    if "$SCRIPT_DIR/deposit_transfer_withdraw.sh" 2>&1 | tee /tmp/midnight_batch_$i.log; then
        # Parse the log to count successful operations
        if grep -q "✓ Deposit successful" /tmp/midnight_batch_$i.log; then
            SUCCESSFUL_DEPOSITS=$((SUCCESSFUL_DEPOSITS + 1))
        fi
        if grep -q "✓ Transfer successful" /tmp/midnight_batch_$i.log; then
            SUCCESSFUL_TRANSFERS=$((SUCCESSFUL_TRANSFERS + 1))
        fi
        if grep -q "✓ Withdrawal successful" /tmp/midnight_batch_$i.log; then
            SUCCESSFUL_WITHDRAWALS=$((SUCCESSFUL_WITHDRAWALS + 1))
        fi
        
        echo -e "${GREEN}✓ Iteration $i completed successfully${NC}"
    else
        echo -e "${RED}✗ Iteration $i failed${NC}"
        FAILED_ITERATIONS=$((FAILED_ITERATIONS + 1))
    fi
    
    # Clean up temp log
    rm -f /tmp/midnight_batch_$i.log
    
    # Show progress
    echo ""
    echo -e "${BLUE}Progress: $i/$ITERATIONS iterations completed${NC}"
    echo -e "  ${GREEN}✓ Deposits: $SUCCESSFUL_DEPOSITS${NC}"
    echo -e "  ${GREEN}✓ Transfers: $SUCCESSFUL_TRANSFERS${NC}"
    echo -e "  ${GREEN}✓ Withdrawals: $SUCCESSFUL_WITHDRAWALS${NC}"
    if [ $FAILED_ITERATIONS -gt 0 ]; then
        echo -e "  ${RED}✗ Failed: $FAILED_ITERATIONS${NC}"
    fi
    echo ""
    
    # Delay before next iteration (skip on last iteration)
    if [ $i -lt $ITERATIONS ]; then
        echo -e "${YELLOW}Waiting ${DELAY}s before next iteration...${NC}"
        sleep $DELAY
        echo ""
    fi
done

END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))

# Final summary
echo -e "${CYAN}════════════════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Batch Transaction Summary${NC}"
echo -e "${CYAN}════════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "${BLUE}Execution Summary:${NC}"
echo "  Total iterations: $ITERATIONS"
echo "  Duration: ${DURATION}s"
echo "  Average time per iteration: $((DURATION / ITERATIONS))s"
echo ""
echo -e "${BLUE}Transaction Results:${NC}"
echo -e "  ${GREEN}Successful deposits: $SUCCESSFUL_DEPOSITS / $ITERATIONS${NC}"
echo -e "  ${GREEN}Successful transfers: $SUCCESSFUL_TRANSFERS / $ITERATIONS${NC}"
echo -e "  ${GREEN}Successful withdrawals: $SUCCESSFUL_WITHDRAWALS / $ITERATIONS${NC}"
if [ $FAILED_ITERATIONS -gt 0 ]; then
    echo -e "  ${RED}Failed iterations: $FAILED_ITERATIONS / $ITERATIONS${NC}"
fi
echo ""

# Show final pool stats
echo -e "${YELLOW}Final Pool Statistics:${NC}"
FINAL_STATS=$(get_stats)
echo "$FINAL_STATS" | jq -C '.' 2>/dev/null || echo "$FINAL_STATS"
echo ""

# Calculate and display differences
if command -v jq &> /dev/null; then
    echo -e "${YELLOW}Changes:${NC}"
    
    INITIAL_POOL=$(echo "$INITIAL_STATS" | jq -r '.pool_balance // 0')
    FINAL_POOL=$(echo "$FINAL_STATS" | jq -r '.pool_balance // 0')
    POOL_CHANGE=$((FINAL_POOL - INITIAL_POOL))
    
    INITIAL_DEPOSITS=$(echo "$INITIAL_STATS" | jq -r '.deposit_count // 0')
    FINAL_DEPOSITS=$(echo "$FINAL_STATS" | jq -r '.deposit_count // 0')
    DEPOSITS_CHANGE=$((FINAL_DEPOSITS - INITIAL_DEPOSITS))
    
    INITIAL_WITHDRAWALS=$(echo "$INITIAL_STATS" | jq -r '.withdraw_count // 0')
    FINAL_WITHDRAWALS=$(echo "$FINAL_STATS" | jq -r '.withdraw_count // 0')
    WITHDRAWALS_CHANGE=$((FINAL_WITHDRAWALS - INITIAL_WITHDRAWALS))
    
    INITIAL_NOTES=$(echo "$INITIAL_STATS" | jq -r '.total_notes // 0')
    FINAL_NOTES=$(echo "$FINAL_STATS" | jq -r '.total_notes // 0')
    NOTES_CHANGE=$((FINAL_NOTES - INITIAL_NOTES))
    
    echo "  Pool balance: $INITIAL_POOL → $FINAL_POOL (${POOL_CHANGE:+}$POOL_CHANGE)"
    echo "  Deposits: $INITIAL_DEPOSITS → $FINAL_DEPOSITS (+$DEPOSITS_CHANGE)"
    echo "  Withdrawals: $INITIAL_WITHDRAWALS → $FINAL_WITHDRAWALS (+$WITHDRAWALS_CHANGE)"
    echo "  Total notes: $INITIAL_NOTES → $FINAL_NOTES (+$NOTES_CHANGE)"
    echo ""
fi

# Success summary
if [ $FAILED_ITERATIONS -eq 0 ]; then
    echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║  ✓ All $ITERATIONS iterations completed successfully!              ║${NC}"
    echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
    exit 0
else
    echo -e "${YELLOW}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${YELLOW}║  ⚠ Completed with $FAILED_ITERATIONS failed iteration(s)                   ║${NC}"
    echo -e "${YELLOW}╔════════════════════════════════════════════════════════════════╗${NC}"
    exit 1
fi

