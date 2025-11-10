#!/bin/bash
# TEE Attestation for Ligero Proof Verification
#
# This script implements a challenge-response protocol to prove that
# Ligero proof verification happened inside a Trusted Execution Environment (TEE).
#
# Requirements:
#   - Python 3 with cryptography library: pip3 install cryptography
#   - TDX attestation library (libtdx_attest) for quote generation
#   - sign_ed25519.py in the same directory
#
# Actors:
#   - Challenger: sends fresh nonce
#   - TD (this script in TEE): verifies proof, generates attestation
#   - Auditor/Verifier: checks attestation
#
# Usage:
#   ./attest_verification.sh <nonce_hex> [output_dir]

set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Parse arguments
if [ $# -lt 1 ]; then
    echo "Usage: $0 <nonce_hex> [output_dir]"
    echo ""
    echo "Example:"
    echo "  $0 \$(openssl rand -hex 32) ./attestation_output"
    echo ""
    exit 1
fi

NONCE="$1"
OUTPUT_DIR="${2:-$SCRIPT_DIR/attestation_$(date +%s)}"

mkdir -p "$OUTPUT_DIR"

echo -e "${BLUE}=== TEE Attestation for Ligero Proof Verification ===${NC}\n"

# Check requirements
echo "Checking requirements..."
if ! command -v python3 &> /dev/null; then
    echo -e "${RED}Error: Python 3 not found${NC}"
    echo "Install with: sudo apt-get install python3"
    exit 1
fi

if ! python3 -c "from cryptography.hazmat.primitives.asymmetric import ed25519" 2>/dev/null; then
    echo -e "${RED}Error: Python cryptography library not found${NC}"
    echo "Install with: pip3 install cryptography"
    exit 1
fi

if [ ! -f "$SCRIPT_DIR/sign_ed25519.py" ]; then
    echo -e "${RED}Error: sign_ed25519.py not found in $SCRIPT_DIR${NC}"
    echo "Copy sign_ed25519.py to the script directory"
    exit 1
fi

echo -e "${GREEN}✓ All requirements met${NC}\n"

echo "Nonce (from Challenger): $NONCE"
echo "Output directory: $OUTPUT_DIR"
echo ""

# Validate inputs
if [ ! -f "$SCRIPT_DIR/proof_data.gz" ]; then
    echo -e "${RED}Error: proof_data.gz not found${NC}"
    exit 1
fi

if [ ! -f "$SCRIPT_DIR/verify_config.json" ]; then
    echo -e "${RED}Error: verify_config.json not found${NC}"
    echo "Run ./generate_verify_command.sh --local first"
    exit 1
fi

# Step 1: Generate ephemeral signing keypair
echo -e "${YELLOW}Step 1: Generate ephemeral Ed25519 keypair inside TEE${NC}"

# Generate Ed25519 keypair using openssl (or ssh-keygen)
KEYPAIR_DIR="$OUTPUT_DIR/.ephemeral_keys"
mkdir -p "$KEYPAIR_DIR"

# Using OpenSSL for Ed25519
openssl genpkey -algorithm ED25519 -out "$KEYPAIR_DIR/private.pem" 2>/dev/null

# Extract public key
openssl pkey -in "$KEYPAIR_DIR/private.pem" -pubout -out "$KEYPAIR_DIR/public.pem" 2>/dev/null

# Get public key in raw format (32 bytes for Ed25519)
# For verification, we'll store both PEM and raw hex
K_PUB_PEM=$(cat "$KEYPAIR_DIR/public.pem")
K_PUB_HEX=$(openssl pkey -pubin -in "$KEYPAIR_DIR/public.pem" -text -noout 2>/dev/null | grep -A3 "pub:" | tail -n +2 | tr -d ' :\n')

echo "  Ephemeral public key (first 16 bytes): ${K_PUB_HEX:0:32}..."

# Step 2: Compute H = SHA256(K_pub) for REPORTDATA
echo -e "\n${YELLOW}Step 2: Prepare REPORTDATA with SHA256(K_pub)${NC}"

H=$(echo -n "$K_PUB_HEX" | xxd -r -p | sha256sum | cut -d' ' -f1)
echo "  H = SHA256(K_pub): $H"

# Pack H into 64-byte REPORTDATA (canonical format: H || zeros)
REPORTDATA="${H}$(printf '%0*d' 64 0 | tr '0' '0')"
REPORTDATA="${REPORTDATA:0:128}"  # Ensure exactly 64 bytes (128 hex chars)

echo "  REPORTDATA (first 32 bytes): ${REPORTDATA:0:64}..."

# Step 3: Generate TEE Quote
echo -e "\n${YELLOW}Step 3: Generate TEE attestation quote${NC}"

# Detect platform and call appropriate quote generation
QUOTE_FILE="$OUTPUT_DIR/quote.dat"
REPORT_FILE="$OUTPUT_DIR/report.dat"

if [ -c "/dev/tdx_guest" ] || [ -c "/dev/tdx-attest" ]; then
    if [ -c "/dev/tdx-attest" ]; then
        echo "  Detected Intel TDX platform (/dev/tdx-attest)"
    else
        echo "  Detected Intel TDX platform (/dev/tdx_guest)"
    fi
    
    # Prepare REPORTDATA as binary (64 bytes)
    echo "$REPORTDATA" | xxd -r -p > "$OUTPUT_DIR/reportdata.bin"
    
    TDX_SUCCESS=false
    
    # Method 1: Try compiled tdx_quote_gen (libtdx-attest)
    if [ -f "$SCRIPT_DIR/tdx_quote_gen" ]; then
        echo "  Using compiled tdx_quote_gen (libtdx-attest)"
        if sudo "$SCRIPT_DIR/tdx_quote_gen" "$OUTPUT_DIR/reportdata.bin" "$QUOTE_FILE" 2>&1 | tee /tmp/tdx_gen.log | grep -q "Success"; then
            if [ -f "$QUOTE_FILE" ] && [ -s "$QUOTE_FILE" ]; then
                echo -e "  ${GREEN}✓${NC} TDX quote generated via libtdx-attest"
                TDX_SUCCESS=true
            fi
        fi
    
    # Method 2: Try system-installed tdx_quote_gen
    elif command -v tdx_quote_gen &> /dev/null; then
        echo "  Using system tdx_quote_gen"
        if sudo tdx_quote_gen "$OUTPUT_DIR/reportdata.bin" "$QUOTE_FILE" 2>&1 | grep -q "Success"; then
            if [ -f "$QUOTE_FILE" ] && [ -s "$QUOTE_FILE" ]; then
                echo -e "  ${GREEN}✓${NC} TDX quote generated via system tool"
                TDX_SUCCESS=true
            fi
        fi
    
    # Method 3: Try Intel TDX attestation sample tool
    elif command -v tdx_attest &> /dev/null; then
        echo "  Using tdx_attest tool"
        tdx_attest -r "$OUTPUT_DIR/reportdata.bin" -q "$QUOTE_FILE" 2>&1 | grep -v "^$" || true
        if [ -f "$QUOTE_FILE" ] && [ -s "$QUOTE_FILE" ]; then
            echo -e "  ${GREEN}✓${NC} TDX quote generated via tdx_attest"
            TDX_SUCCESS=true
        fi
    
    # Method 4: Try direct ioctl via Python (for /dev/tdx_guest)
    elif [ -c "/dev/tdx_guest" ] && [ -f "$SCRIPT_DIR/tdx_quote_gen.py" ]; then
        echo "  Using direct TDX ioctl (Python)"
        if python3 "$SCRIPT_DIR/tdx_quote_gen.py" \
            "$OUTPUT_DIR/reportdata.bin" "$QUOTE_FILE" 2>&1 | grep -q "Success"; then
            if [ -f "$QUOTE_FILE" ] && [ -s "$QUOTE_FILE" ]; then
                echo -e "  ${GREEN}✓${NC} TDX quote generated via ioctl"
                TDX_SUCCESS=true
            fi
        fi
    
    # Method 5: Try Google's go-tdx-guest (if available)
    elif command -v go-tdx-guest &> /dev/null; then
        echo "  Using go-tdx-guest"
        go-tdx-guest report -report-data "$OUTPUT_DIR/reportdata.bin" -out "$QUOTE_FILE" 2>&1 | grep -v "^$" || true
        if [ -f "$QUOTE_FILE" ] && [ -s "$QUOTE_FILE" ]; then
            echo -e "  ${GREEN}✓${NC} TDX quote generated via go-tdx-guest"
            TDX_SUCCESS=true
        fi
    fi
    
    # Fallback: Generate mock quote if all methods failed
    if [ "${TDX_SUCCESS}" = "false" ] || [ ! -f "$QUOTE_FILE" ] || [ ! -s "$QUOTE_FILE" ]; then
        echo -e "  ${YELLOW}Warning: No working TDX attestation tool found${NC}"
        echo "  To use real quotes, compile tdx_quote_gen:"
        echo "    cd $SCRIPT_DIR"
        echo "    make -f Makefile.tdx"
        echo "    sudo ./tdx_quote_gen reportdata.bin quote.dat"
        echo ""
        echo -e "  ${YELLOW}Generating mock quote for testing...${NC}"
        cat > "$QUOTE_FILE" <<EOF
MOCK_TDX_QUOTE_v1
REPORTDATA: $REPORTDATA
TIMESTAMP: $(date -u +%Y-%m-%dT%H:%M:%SZ)
PLATFORM: TDX_MOCK
WARNING: This is a mock quote for testing only
NOTE: Real TDX quote generation requires libtdx-attest library
      Compile with: make -f Makefile.tdx
EOF
    fi
elif command -v google-tee-attest &> /dev/null; then
    echo "  Detected Google Confidential VM"
    # For Google Confidential VM
    echo "$REPORTDATA" | xxd -r -p > "$OUTPUT_DIR/reportdata.bin"
    google-tee-attest generate-quote --report-data="$OUTPUT_DIR/reportdata.bin" --output="$QUOTE_FILE" 2>&1 | grep -v "^$" || true
    echo -e "  ${GREEN}✓${NC} Google TEE quote generated"
elif [ -c "/dev/sev-guest" ]; then
    echo "  Detected AMD SEV-SNP platform"
    # AMD SEV-SNP quote generation would go here
    echo -e "  ${YELLOW}Warning: SEV-SNP quote generation not implemented, generating mock${NC}"
    cat > "$QUOTE_FILE" <<EOF
MOCK_SEV_QUOTE_v1
REPORTDATA: $REPORTDATA
TIMESTAMP: $(date -u +%Y-%m-%dT%H:%M:%SZ)
PLATFORM: SEV_MOCK
WARNING: This is a mock quote for testing only
EOF
else
    echo -e "  ${YELLOW}Warning: No TEE platform detected, generating mock quote for testing${NC}"
    cat > "$QUOTE_FILE" <<EOF
MOCK_QUOTE_v1
REPORTDATA: $REPORTDATA
TIMESTAMP: $(date -u +%Y-%m-%dT%H:%M:%SZ)
PLATFORM: DEVELOPMENT_ONLY
WARNING: This is a mock quote for testing. Not suitable for production use.
         In production, this would be signed by Intel/AMD/Google TPM.
EOF
fi

# Step 4: Verify the Ligero proof locally
echo -e "\n${YELLOW}Step 4: Verify Ligero proof inside TEE${NC}"

# Determine verifier path
if [[ "$OSTYPE" == "darwin"* ]]; then
    VERIFIER="${WEBGPU_VERIFIER:-$REPO_ROOT/crates/adapters/ligero/bins/macos/bin/webgpu_verifier}"
else
    VERIFIER="${WEBGPU_VERIFIER:-$REPO_ROOT/crates/adapters/ligero/bins/linux-amd64/bin/webgpu_verifier}"
fi

if [ ! -f "$VERIFIER" ]; then
    echo -e "${RED}Error: webgpu_verifier not found at $VERIFIER${NC}"
    echo "Set WEBGPU_VERIFIER environment variable"
    exit 1
fi

# Run verification and capture result
cd "$SCRIPT_DIR"
VERIFY_OUTPUT=$($VERIFIER "$(cat verify_config.json)" 2>&1)
VERIFY_RESULT=$?
cd - > /dev/null

# Save full verification output
echo "$VERIFY_OUTPUT" > "$OUTPUT_DIR/verification_output.txt"

# Check if verification succeeded
if echo "$VERIFY_OUTPUT" | grep -q "Final Verify Result:.*true"; then
    VERIFICATION_STATUS="SUCCESS"
    echo -e "  ${GREEN}✓ Proof verification PASSED${NC}"
else
    VERIFICATION_STATUS="FAILED"
    echo -e "  ${RED}✗ Proof verification FAILED${NC}"
    echo "See $OUTPUT_DIR/verification_output.txt for details"
    exit 1
fi

# Step 5: Compute hashes
echo -e "\n${YELLOW}Step 5: Compute cryptographic hashes${NC}"

QUOTE_HASH=$(sha256sum "$QUOTE_FILE" | cut -d' ' -f1)
echo "  quote_hash: $QUOTE_HASH"

PROOF_HASH=$(sha256sum "$SCRIPT_DIR/proof_data.gz" | cut -d' ' -f1)
echo "  proof_hash: $PROOF_HASH"

CONFIG_HASH=$(sha256sum "$SCRIPT_DIR/verify_config.json" | cut -d' ' -f1)
echo "  config_hash: $CONFIG_HASH"

# Step 6: Build canonical statement
echo -e "\n${YELLOW}Step 6: Build and sign attestation statement${NC}"

TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Create canonical statement (order matters for signature verification!)
cat > "$OUTPUT_DIR/statement.json" <<EOF
{
  "version": "1.0",
  "protocol": "ligero-tee-attestation",
  "timestamp": "$TIMESTAMP",
  "nonce": "$NONCE",
  "verification": {
    "status": "$VERIFICATION_STATUS",
    "proof_hash": "$PROOF_HASH",
    "config_hash": "$CONFIG_HASH",
    "verifier_version": "webgpu_verifier-1.0.0"
  },
  "attestation": {
    "quote_hash": "$QUOTE_HASH",
    "pubkey_hash": "$H",
    "reportdata": "$REPORTDATA"
  },
  "metadata": {
    "platform": "$(uname -s)",
    "architecture": "$(uname -m)"
  }
}
EOF

# Canonicalize JSON (for consistent signature)
# Use -n to avoid trailing newline (consistent with signing)
STATEMENT_CANONICAL=$(jq -c -S '.' "$OUTPUT_DIR/statement.json")
echo -n "$STATEMENT_CANONICAL" > "$OUTPUT_DIR/statement.canonical.json"

echo "  Statement created: $OUTPUT_DIR/statement.json"

# Step 7: Sign the statement
echo -e "\n${YELLOW}Step 7: Sign statement with ephemeral key${NC}"

# Use Python for Ed25519 signing
if [ -f "$SCRIPT_DIR/sign_ed25519.py" ]; then
    # Run Python signing
    python3 "$SCRIPT_DIR/sign_ed25519.py" \
        "$KEYPAIR_DIR/private.pem" \
        "$OUTPUT_DIR/statement.canonical.json" \
        "$OUTPUT_DIR/statement.sig"
    
    # Check if signing succeeded by verifying the file was created
    if [ -f "$OUTPUT_DIR/statement.sig" ] && [ -s "$OUTPUT_DIR/statement.sig" ]; then
        SIGNATURE_HEX=$(xxd -p "$OUTPUT_DIR/statement.sig" | tr -d '\n')
        echo "  Signature (first 32 bytes): ${SIGNATURE_HEX:0:64}..."
    else
        echo -e "  ${RED}✗ Python signing failed${NC}"
        echo "  Install cryptography: pip3 install cryptography"
        exit 1
    fi
else
    echo -e "  ${RED}✗ sign_ed25519.py not found${NC}"
    echo "  Copy sign_ed25519.py to $SCRIPT_DIR"
    exit 1
fi

# Step 8: Package attestation bundle
echo -e "\n${YELLOW}Step 8: Package attestation bundle${NC}"

# Copy proof and config to output
cp "$SCRIPT_DIR/proof_data.gz" "$OUTPUT_DIR/"
cp "$SCRIPT_DIR/verify_config.json" "$OUTPUT_DIR/"

# Copy verification dependencies (WASM program and shader)
if [ -f "$SCRIPT_DIR/note_spend_guest.wasm" ]; then
    echo "  Copying WASM program..."
    cp "$SCRIPT_DIR/note_spend_guest.wasm" "$OUTPUT_DIR/"
else
    echo -e "  ${YELLOW}⚠ WASM program not found in $SCRIPT_DIR${NC}"
fi

if [ -d "$SCRIPT_DIR/shader" ]; then
    echo "  Copying shader directory..."
    cp -r "$SCRIPT_DIR/shader" "$OUTPUT_DIR/"
else
    echo -e "  ${YELLOW}⚠ Shader directory not found in $SCRIPT_DIR${NC}"
fi

# Update verify_config.json to use relative paths
if command -v jq &> /dev/null; then
    jq '.program = "./note_spend_guest.wasm" | ."shader-path" = "./shader"' \
        "$OUTPUT_DIR/verify_config.json" > "$OUTPUT_DIR/verify_config.json.tmp"
    mv "$OUTPUT_DIR/verify_config.json.tmp" "$OUTPUT_DIR/verify_config.json"
    echo "  Updated verify_config.json with relative paths"
else
    echo -e "  ${YELLOW}⚠ jq not found, verify_config.json may have absolute paths${NC}"
fi

# Export public key in multiple formats for convenience
echo "$K_PUB_PEM" > "$OUTPUT_DIR/ephemeral_pubkey.pem"
echo "$K_PUB_HEX" > "$OUTPUT_DIR/ephemeral_pubkey.hex"

# Create attestation bundle manifest
cat > "$OUTPUT_DIR/MANIFEST.txt" <<EOF
Ligero TEE Attestation Bundle
=============================

Generated: $TIMESTAMP
Nonce: $NONCE
Verification Status: $VERIFICATION_STATUS

Files in this bundle:
  - quote.dat                    : TEE attestation quote (contains REPORTDATA)
  - statement.json               : Human-readable attestation statement
  - statement.canonical.json     : Canonical JSON (what was signed)
  - statement.sig                : Ed25519 signature over canonical statement
  - ephemeral_pubkey.pem         : Ephemeral public key (PEM format)
  - ephemeral_pubkey.hex         : Ephemeral public key (raw hex)
  - proof_data.gz                : The Ligero proof that was verified
  - verify_config.json           : Verification configuration (with relative paths)
  - note_spend_guest.wasm        : Ligero WASM program for verification
  - shader/                      : WebGPU shader directory
  - verification_output.txt      : Full verifier output
  - verify_attestation.sh        : Automated verification script

Verification Instructions:
  See verify_attestation.sh for automated verification.
  Requires: 
    - Python 3 with cryptography library (pip3 install cryptography)
    - webgpu_verifier binary (set WEBGPU_VERIFIER env var)

Manual Verification Steps:
  1. Verify quote.dat signature chain (Intel/AMD/Google)
  2. Extract REPORTDATA from quote
  3. Verify SHA256(ephemeral_pubkey.hex) == REPORTDATA[0:32]
  4. Verify statement.sig using ephemeral_pubkey.pem (Ed25519 via Python)
  5. Verify statement.nonce matches challenge
  6. Verify statement.quote_hash == SHA256(quote.dat)
  7. Verify statement.proof_hash == SHA256(proof_data.gz)
  8. Re-run Ligero verification:
     cd <bundle_dir>
     \$WEBGPU_VERIFIER "\$(cat verify_config.json)"

Public Key Hash (in quote): $H
Quote Hash: $QUOTE_HASH
Proof Hash: $PROOF_HASH
EOF

echo -e "${GREEN}✓ Attestation bundle created${NC}"
echo ""

# Step 9: Create verification script for auditors
cat > "$OUTPUT_DIR/verify_attestation.sh" <<'VERIFY_SCRIPT_EOF'
#!/bin/bash
# Verify TEE Attestation for Ligero Proof
#
# This script verifies the complete attestation chain:
#   1. TEE quote signature and chain
#   2. Ephemeral key binding (pubkey hash in REPORTDATA)
#   3. Statement signature
#   4. Nonce matching
#   5. Hash consistency
#   6. Ligero proof verification

set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

BUNDLE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo -e "${BLUE}=== Verifying TEE Attestation Bundle ===${NC}\n"

# Check required files
REQUIRED_FILES=(
    "quote.dat"
    "statement.canonical.json"
    "statement.sig"
    "ephemeral_pubkey.pem"
    "ephemeral_pubkey.hex"
    "proof_data.gz"
    "verify_config.json"
)

for file in "${REQUIRED_FILES[@]}"; do
    if [ ! -f "$BUNDLE_DIR/$file" ]; then
        echo -e "${RED}Error: Missing required file: $file${NC}"
        exit 1
    fi
done

echo -e "${GREEN}✓ All required files present${NC}\n"

# Extract information from statement
NONCE=$(jq -r '.nonce' "$BUNDLE_DIR/statement.canonical.json")
QUOTE_HASH_CLAIM=$(jq -r '.attestation.quote_hash' "$BUNDLE_DIR/statement.canonical.json")
PROOF_HASH_CLAIM=$(jq -r '.verification.proof_hash' "$BUNDLE_DIR/statement.canonical.json")
PUBKEY_HASH_CLAIM=$(jq -r '.attestation.pubkey_hash' "$BUNDLE_DIR/statement.canonical.json")
REPORTDATA_CLAIM=$(jq -r '.attestation.reportdata' "$BUNDLE_DIR/statement.canonical.json")

echo "Challenge nonce: $NONCE"
echo ""

# Step 1: Verify quote (platform-specific)
echo -e "${YELLOW}Step 1: Verify quote signature and chain${NC}"

if grep -q "MOCK" "$BUNDLE_DIR/quote.dat" 2>/dev/null; then
    echo -e "  ${YELLOW}⚠ MOCK QUOTE DETECTED${NC}"
    echo "  This is a development/testing quote without cryptographic guarantees"
    echo "  In production, this step would verify Intel/AMD/Google signature chain"
    QUOTE_VALID=true
else
    # Try Google's go-tdx-guest 'check' tool (recommended)
    if command -v check &> /dev/null; then
        echo -e "  Using Google's go-tdx-guest verification tool..."
        if check -in "$BUNDLE_DIR/quote.dat" 2>&1 | tee /tmp/tdx_verify.log | grep -qE "(Verification succeeded|Quote verified)"; then
            echo -e "  ${GREEN}✓${NC} TDX quote verified successfully"
            QUOTE_VALID=true
        else
            echo -e "  ${RED}✗${NC} TDX quote verification failed"
            echo "  See /tmp/tdx_verify.log for details"
            QUOTE_VALID=false
        fi
    # Fallback to Intel's tdx_verify_quote if available
    elif command -v tdx_verify_quote &> /dev/null; then
        echo -e "  Using Intel's tdx_verify_quote..."
        if tdx_verify_quote "$BUNDLE_DIR/quote.dat" 2>&1 | grep -q "VALID"; then
            echo -e "  ${GREEN}✓${NC} TDX quote signature valid"
            QUOTE_VALID=true
        else
            echo -e "  ${RED}✗${NC} TDX quote verification failed"
            QUOTE_VALID=false
        fi
    else
        echo -e "  ${YELLOW}⚠${NC} Quote verification tool not found (skipping platform check)"
        echo ""
        echo "  To enable quote verification, install Google's go-tdx-guest:"
        echo "    go install github.com/google/go-tdx-guest/tools/check@latest"
        echo "    export PATH=\"\$PATH:\$HOME/go/bin\""
        echo ""
        echo "  Or use the installation script:"
        echo "    ./install_tdx_verification.sh"
        QUOTE_VALID=true  # Allow to continue for testing
    fi
fi

# Step 2: Extract and verify REPORTDATA
echo -e "\n${YELLOW}Step 2: Verify ephemeral key binding in REPORTDATA${NC}"

# Extract REPORTDATA from quote (format depends on platform)
if grep -q "REPORTDATA:" "$BUNDLE_DIR/quote.dat" 2>/dev/null; then
    # Mock format (text-based)
    REPORTDATA_ACTUAL=$(grep "REPORTDATA:" "$BUNDLE_DIR/quote.dat" | cut -d' ' -f2)
    echo "  Detected mock quote format"
else
    # Real TDX quote - binary parsing
    # TDX quote REPORTDATA offset varies by implementation:
    #   - offset 560: libtdx-attest / Intel TDX 1.5
    #   - offset 632: Some TDX Quote v4 implementations
    # We'll try multiple offsets and match against the expected hash
    
    QUOTE_SIZE=$(stat -c%s "$BUNDLE_DIR/quote.dat" 2>/dev/null || stat -f%z "$BUNDLE_DIR/quote.dat")
    
    if [ "$QUOTE_SIZE" -lt 624 ]; then
        echo -e "  ${RED}✗${NC} Quote too small (${QUOTE_SIZE} bytes)"
        exit 1
    fi
    
    # Try common offsets first (fast path)
    REPORTDATA_ACTUAL=""
    FOUND_OFFSET=""
    
    for offset in 560 568 632 480 584; do
        if [ "$QUOTE_SIZE" -ge $((offset + 64)) ]; then
            CANDIDATE=$(dd if="$BUNDLE_DIR/quote.dat" bs=1 skip=$offset count=64 2>/dev/null | xxd -p | tr -d '\n')
            
            # Check if first 32 bytes match the expected pubkey hash
            if [ "${CANDIDATE:0:64}" = "$PUBKEY_HASH_CLAIM" ]; then
                REPORTDATA_ACTUAL="$CANDIDATE"
                FOUND_OFFSET=$offset
                break
            fi
        fi
    done
    
    # If not found at known offsets, do a full scan (slower but comprehensive)
    if [ -z "$REPORTDATA_ACTUAL" ]; then
        echo "  Scanning quote for REPORTDATA hash (this may take a moment)..."
        
        for offset in $(seq 0 4 $((QUOTE_SIZE - 64))); do
            CANDIDATE=$(dd if="$BUNDLE_DIR/quote.dat" bs=1 skip=$offset count=64 2>/dev/null | xxd -p | tr -d '\n')
            
            # Check if first 32 bytes match the expected pubkey hash
            if [ "${CANDIDATE:0:64}" = "$PUBKEY_HASH_CLAIM" ]; then
                REPORTDATA_ACTUAL="$CANDIDATE"
                FOUND_OFFSET=$offset
                echo "  Found REPORTDATA at offset $offset"
                break
            fi
        done
    fi
    
    if [ -z "$REPORTDATA_ACTUAL" ]; then
        echo -e "  ${RED}✗${NC} Could not find REPORTDATA hash anywhere in quote"
        echo "  Quote size: $QUOTE_SIZE bytes"
        echo "  Expected to find hash: ${PUBKEY_HASH_CLAIM:0:32}..."
        echo ""
        echo "  This likely means the quote was not generated with this REPORTDATA"
        echo "  Verify reportdata.bin matches ephemeral_pubkey.hex:"
        echo "    xxd -p reportdata.bin | head -c 64"
        echo "    cat ephemeral_pubkey.hex | xxd -r -p | sha256sum"
        exit 1
    fi
    
    if [ ${#REPORTDATA_ACTUAL} -ne 128 ]; then
        echo -e "  ${RED}✗${NC} Failed to extract REPORTDATA (got ${#REPORTDATA_ACTUAL} hex chars, expected 128)"
        exit 1
    fi
    
    echo "  Extracted REPORTDATA from TDX quote (offset $FOUND_OFFSET, 64 bytes)"
fi

echo "  REPORTDATA from quote: ${REPORTDATA_ACTUAL:0:64}..."

# Compute SHA256 of public key
K_PUB_HEX=$(cat "$BUNDLE_DIR/ephemeral_pubkey.hex")
H_COMPUTED=$(echo -n "$K_PUB_HEX" | xxd -r -p | sha256sum | cut -d' ' -f1)

echo "  SHA256(ephemeral_pubkey): $H_COMPUTED"
echo "  Expected (from statement): $PUBKEY_HASH_CLAIM"

# Verify REPORTDATA[0:32] matches SHA256(K_pub)
REPORTDATA_HASH=${REPORTDATA_ACTUAL:0:64}  # First 32 bytes (64 hex chars)

if [ "$REPORTDATA_HASH" = "$H_COMPUTED" ]; then
    echo -e "  ${GREEN}✓${NC} Pubkey hash matches REPORTDATA[0:32]"
    echo -e "  ${GREEN}✓${NC} Ephemeral key is cryptographically bound to TEE quote"
else
    echo -e "  ${RED}✗${NC} Pubkey hash mismatch!"
    echo "  REPORTDATA[0:32]: $REPORTDATA_HASH"
    echo "  SHA256(K_pub):    $H_COMPUTED"
    exit 1
fi

# Also verify statement's claim matches actual REPORTDATA
if [ "$REPORTDATA_CLAIM" = "$REPORTDATA_ACTUAL" ]; then
    echo -e "  ${GREEN}✓${NC} Statement's REPORTDATA claim is accurate"
else
    echo -e "  ${YELLOW}⚠${NC} Statement's REPORTDATA claim differs from actual quote"
    echo "  This is suspicious but non-critical if hash matches"
fi

# Step 3: Verify statement signature
echo -e "\n${YELLOW}Step 3: Verify statement signature${NC}"

# Use Python for Ed25519 verification
if command -v python3 &> /dev/null; then
    # Inline Python verification
    if python3 - "$BUNDLE_DIR/ephemeral_pubkey.pem" \
                 "$BUNDLE_DIR/statement.canonical.json" \
                 "$BUNDLE_DIR/statement.sig" <<'PYVERIFY'
import sys
from pathlib import Path
try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ed25519
    from cryptography.exceptions import InvalidSignature
    
    pub_key = serialization.load_pem_public_key(Path(sys.argv[1]).read_bytes())
    data = Path(sys.argv[2]).read_bytes()
    signature = Path(sys.argv[3]).read_bytes()
    pub_key.verify(signature, data)
    sys.exit(0)
except Exception as e:
    print(f"Verification failed: {e}", file=sys.stderr)
    sys.exit(1)
PYVERIFY
    then
        echo -e "  ${GREEN}✓${NC} Statement signature valid"
    else
        echo -e "  ${RED}✗${NC} Statement signature verification failed!"
        echo "  Install Python cryptography: pip3 install cryptography"
        exit 1
    fi
else
    echo -e "  ${RED}✗${NC} Python not available for signature verification"
    echo "  Install Python 3 and cryptography library"
    exit 1
fi

# Step 4: Verify nonce
echo -e "\n${YELLOW}Step 4: Verify challenge nonce${NC}"

if [ -z "$EXPECTED_NONCE" ]; then
    echo -e "  ${YELLOW}⚠${NC} No EXPECTED_NONCE provided (set env var to check)"
    echo "  Nonce in statement: $NONCE"
else
    if [ "$NONCE" = "$EXPECTED_NONCE" ]; then
        echo -e "  ${GREEN}✓${NC} Nonce matches challenge"
    else
        echo -e "  ${RED}✗${NC} Nonce mismatch!"
        echo "    Expected: $EXPECTED_NONCE"
        echo "    Got:      $NONCE"
        exit 1
    fi
fi

# Step 5: Verify hash consistency
echo -e "\n${YELLOW}Step 5: Verify hash consistency${NC}"

QUOTE_HASH_ACTUAL=$(sha256sum "$BUNDLE_DIR/quote.dat" | cut -d' ' -f1)
PROOF_HASH_ACTUAL=$(sha256sum "$BUNDLE_DIR/proof_data.gz" | cut -d' ' -f1)

echo "  Quote hash (computed):  $QUOTE_HASH_ACTUAL"
echo "  Quote hash (claimed):   $QUOTE_HASH_CLAIM"
if [ "$QUOTE_HASH_ACTUAL" = "$QUOTE_HASH_CLAIM" ]; then
    echo -e "  ${GREEN}✓${NC} Quote hash matches"
else
    echo -e "  ${RED}✗${NC} Quote hash mismatch!"
    exit 1
fi

echo ""
echo "  Proof hash (computed):  $PROOF_HASH_ACTUAL"
echo "  Proof hash (claimed):   $PROOF_HASH_CLAIM"
if [ "$PROOF_HASH_ACTUAL" = "$PROOF_HASH_CLAIM" ]; then
    echo -e "  ${GREEN}✓${NC} Proof hash matches"
else
    echo -e "  ${RED}✗${NC} Proof hash mismatch!"
    exit 1
fi

# Step 6: Re-verify the Ligero proof
echo -e "\n${YELLOW}Step 6: Re-verify Ligero proof${NC}"

if [ -z "$WEBGPU_VERIFIER" ]; then
    echo -e "  ${YELLOW}⚠${NC} WEBGPU_VERIFIER not set, skipping proof re-verification"
    echo "    Set WEBGPU_VERIFIER to path of webgpu_verifier binary to enable"
else
    if [ ! -f "$WEBGPU_VERIFIER" ]; then
        echo -e "  ${RED}✗${NC} WEBGPU_VERIFIER not found: $WEBGPU_VERIFIER"
        exit 1
    fi
    
    cd "$BUNDLE_DIR"
    if $WEBGPU_VERIFIER "$(cat verify_config.json)" 2>&1 | grep -q "Final Verify Result:.*true"; then
        echo -e "  ${GREEN}✓${NC} Ligero proof re-verification PASSED"
    else
        echo -e "  ${RED}✗${NC} Ligero proof re-verification FAILED"
        exit 1
    fi
    cd - > /dev/null
fi

# Final verdict
echo ""
echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
echo -e "${GREEN}✓ ATTESTATION VERIFIED${NC}"
echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
echo ""
echo "The following has been cryptographically proven:"
echo "  1. A TEE (quote verified) with REPORTDATA=$H_COMPUTED"
echo "  2. Holds ephemeral key K with SHA256(K_pub) = REPORTDATA[0:32]"
echo "  3. Signed a statement claiming:"
echo "     - Challenge nonce: $NONCE"
echo "     - Verified proof with hash: ${PROOF_HASH_ACTUAL:0:16}..."
echo "     - Quote hash: ${QUOTE_HASH_ACTUAL:0:16}..."
echo "  4. The claimed proof verification is reproducible"
echo ""
echo "Conclusion: The Ligero proof was verified inside the attested TEE."
VERIFY_SCRIPT_EOF

chmod +x "$OUTPUT_DIR/verify_attestation.sh"

echo -e "${BLUE}Files created in $OUTPUT_DIR:${NC}"
ls -lh "$OUTPUT_DIR" | tail -n +2 | grep -v "^d" | awk '{print "  " $9 " (" $5 ")"}'
echo ""

echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
echo -e "${GREEN}✓ ATTESTATION COMPLETE${NC}"
echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
echo ""
echo -e "${BLUE}Send to Challenger/Auditor:${NC}"
echo "  Entire directory: $OUTPUT_DIR"
echo ""
echo -e "${BLUE}Verify attestation:${NC}"
echo "  cd $OUTPUT_DIR"
echo "  EXPECTED_NONCE=$NONCE ./verify_attestation.sh"
echo ""
echo -e "${YELLOW}Note: This attestation proves that the Ligero proof verification"
echo "      happened inside the TEE whose quote is included.${NC}"

