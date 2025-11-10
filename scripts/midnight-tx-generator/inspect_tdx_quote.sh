#!/bin/bash
# Inspect Intel TDX Quote Structure
#
# This script parses and displays the contents of a TDX quote binary file.
# Useful for debugging attestation issues and understanding quote format.

set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

if [ $# -ne 1 ]; then
    echo "Usage: $0 <quote.dat>"
    echo ""
    echo "Example:"
    echo "  $0 attestation_output/quote.dat"
    exit 1
fi

QUOTE_FILE="$1"

if [ ! -f "$QUOTE_FILE" ]; then
    echo "Error: Quote file not found: $QUOTE_FILE"
    exit 1
fi

echo -e "${BLUE}═══════════════════════════════════════════════${NC}"
echo -e "${BLUE}    Intel TDX Quote Inspector${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════${NC}"
echo ""

# Check if this is a mock quote
if grep -q "REPORTDATA:" "$QUOTE_FILE" 2>/dev/null; then
    echo -e "${YELLOW}⚠ MOCK QUOTE DETECTED${NC}"
    echo ""
    grep "MOCK QUOTE" "$QUOTE_FILE" || true
    grep "REPORTDATA:" "$QUOTE_FILE" || true
    exit 0
fi

# Get quote size
QUOTE_SIZE=$(stat -c%s "$QUOTE_FILE" 2>/dev/null || stat -f%z "$QUOTE_FILE")
echo -e "${CYAN}File Information:${NC}"
echo "  Path: $QUOTE_FILE"
echo "  Size: $QUOTE_SIZE bytes"
echo ""

if [ "$QUOTE_SIZE" -lt 696 ]; then
    echo -e "${YELLOW}⚠ Quote too small (expected at least 696 bytes for TDX v4)${NC}"
    exit 1
fi

# Extract and display quote fields
# TDX Quote v4 Structure (simplified):
# Offset  Size  Field
# ------  ----  -----
# 0       2     Version
# 2       2     Attestation Key Type
# 4       4     TEE Type (0x00000081 = TDX)
# 48      48    QE SVN (Security Version Numbers)
# 96      32    PCE ID
# 128     16    QE Vendor ID
# 144     20    User Data (unused in v4)
# 164     468   TD Report (contains measurements and REPORTDATA)
#   - 584  48   MRTD (Measurement of TD)
#   - 632  64   REPORTDATA (user-provided data)

echo -e "${CYAN}Quote Header:${NC}"

# Version (offset 0, 2 bytes)
VERSION=$(dd if="$QUOTE_FILE" bs=1 skip=0 count=2 2>/dev/null | xxd -p)
echo "  Version: 0x$VERSION"

# TEE Type (offset 4, 4 bytes, little-endian)
TEE_TYPE=$(dd if="$QUOTE_FILE" bs=1 skip=4 count=4 2>/dev/null | xxd -p)
case "$TEE_TYPE" in
    "81000000") echo "  TEE Type: Intel TDX (0x00000081)" ;;
    "00000000") echo "  TEE Type: SGX (0x00000000)" ;;
    *) echo "  TEE Type: Unknown (0x$TEE_TYPE)" ;;
esac

echo ""
echo -e "${CYAN}TD Report (Trust Domain):${NC}"

# MRTD (Measurement of TD) - offset 584, 48 bytes
if [ "$QUOTE_SIZE" -ge 632 ]; then
    MRTD=$(dd if="$QUOTE_FILE" bs=1 skip=584 count=48 2>/dev/null | xxd -p | tr -d '\n')
    echo "  MRTD (TD Measurement):"
    echo "    ${MRTD:0:64}"
    echo "    ${MRTD:64:64}"
    echo "    ${MRTD:128:64}"
fi

echo ""
echo -e "${CYAN}REPORTDATA (User Data):${NC}"

# REPORTDATA - offset 632, 64 bytes
if [ "$QUOTE_SIZE" -ge 696 ]; then
    REPORTDATA=$(dd if="$QUOTE_FILE" bs=1 skip=632 count=64 2>/dev/null | xxd -p | tr -d '\n')
    echo "  Full REPORTDATA (64 bytes):"
    echo "    ${REPORTDATA:0:64}"
    echo "    ${REPORTDATA:64:64}"
    echo ""
    echo "  REPORTDATA[0:32] (typically SHA256(pubkey)):"
    echo "    ${REPORTDATA:0:64}"
    echo ""
    echo "  REPORTDATA[32:64] (padding/unused):"
    echo "    ${REPORTDATA:64:64}"
    
    # Check if second half is zeros
    if echo "${REPORTDATA:64:64}" | grep -qE '^0+$'; then
        echo -e "    ${GREEN}✓${NC} Properly zero-padded"
    else
        echo -e "    ${YELLOW}⚠${NC} Non-zero padding (unusual)"
    fi
fi

echo ""
echo -e "${CYAN}Quote Signature:${NC}"

# Signature data starts after fixed header (offset 696+)
if [ "$QUOTE_SIZE" -gt 696 ]; then
    SIG_SIZE=$((QUOTE_SIZE - 696))
    echo "  Signature data size: $SIG_SIZE bytes"
    echo "  Signature type: ECDSA P-256 (Intel QE)"
else
    echo "  No signature data found"
fi

echo ""
echo -e "${CYAN}Verification:${NC}"
echo "  To verify this quote signature:"
echo "    check -in $QUOTE_FILE"
echo ""
echo "  Or use Intel's verification:"
echo "    tdx_verify_quote $QUOTE_FILE"

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════${NC}"

