#!/usr/bin/env bash
# Build script for note-spend-guest WASM module
# 
# This script:
# 1. Ensures the wasm32-wasip1 target is installed
# 2. Builds the guest program with release optimizations
# 3. Runs wasm-opt for further size reduction
# 4. Copies the WASM binary to the bins/programs directory
# 5. Optionally generates a WAT (WebAssembly Text) file for inspection
#
# Usage:
#   ./build.sh          # Build and copy WASM
#   ./build.sh --wat    # Build, copy WASM, and generate WAT
#   ./build.sh --debug  # Build with diagnostics feature enabled

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

echo -e "${BLUE}Building note-spend-guest...${NC}"

# Parse arguments
FEATURES=""
GENERATE_WAT=false
for arg in "$@"; do
    case $arg in
        --wat|-w)
            GENERATE_WAT=true
            ;;
        --debug)
            FEATURES="--features diagnostics"
            echo -e "${YELLOW}Building with diagnostics feature enabled${NC}"
            ;;
    esac
done

# Ensure the wasm32-wasip1 target is installed
echo "Checking for wasm32-wasip1 target..."
if ! rustup target list | grep -q "wasm32-wasip1 (installed)"; then
    echo "Installing wasm32-wasip1 target..."
    rustup target add wasm32-wasip1
fi

# Build with optimizations
echo "Building WASM module..."
RUSTFLAGS="-C link-arg=-s" cargo build --target wasm32-wasip1 --release $FEATURES

# Output directory
OUT_DIR="../bins/programs"
mkdir -p "$OUT_DIR"

# Copy the WASM binary
WASM_FILE="target/wasm32-wasip1/release/note_spend_guest.wasm"

# Apply wasm-opt if available for additional size reduction
if command -v wasm-opt &> /dev/null; then
    echo "Optimizing with wasm-opt..."
    # --enable-bulk-memory required for memory.copy/fill operations
    wasm-opt --enable-bulk-memory -Oz -o "$OUT_DIR/note_spend_guest.wasm" "$WASM_FILE"
    
    # Show size comparison
    ORIG_SIZE=$(ls -lh "$WASM_FILE" | awk '{print $5}')
    OPT_SIZE=$(ls -lh "$OUT_DIR/note_spend_guest.wasm" | awk '{print $5}')
    echo -e "${GREEN}✓ WASM optimized: $ORIG_SIZE → $OPT_SIZE${NC}"
else
    cp "$WASM_FILE" "$OUT_DIR/note_spend_guest.wasm"
    echo -e "${YELLOW}Warning: wasm-opt not found. Install binaryen for smaller binaries.${NC}"
    echo "  brew install binaryen  # macOS"
    echo "  apt install binaryen   # Debian/Ubuntu"
fi

# Strip custom sections if wasm-strip is available
if command -v wasm-strip &> /dev/null; then
    wasm-strip "$OUT_DIR/note_spend_guest.wasm" 2>/dev/null || true
fi

echo -e "${GREEN}✓ WASM binary built: $OUT_DIR/note_spend_guest.wasm${NC}"

# Get final size info
SIZE=$(ls -lh "$OUT_DIR/note_spend_guest.wasm" | awk '{print $5}')
echo "  Final size: $SIZE"

# Generate WAT if requested
if [[ "$GENERATE_WAT" == "true" ]]; then
    echo "Generating WAT (WebAssembly Text) file..."
    if command -v wasm2wat &> /dev/null; then
        wasm2wat "$OUT_DIR/note_spend_guest.wasm" -o "$OUT_DIR/note_spend_guest.wat"
        echo -e "${GREEN}✓ WAT file generated: $OUT_DIR/note_spend_guest.wat${NC}"
    else
        echo "Warning: wasm2wat not found. Install WABT to generate WAT files."
        echo "  https://github.com/WebAssembly/wabt"
    fi
fi

echo -e "${GREEN}Build complete!${NC}"

