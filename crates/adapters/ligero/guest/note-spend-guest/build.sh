#!/usr/bin/env bash
# Build script for note-spend-guest WASM module
# 
# This script:
# 1. Ensures the wasm32-wasip1 target is installed
# 2. Builds the guest program with release optimizations
# 3. Copies the WASM binary to the bins/programs directory
# 4. Optionally generates a WAT (WebAssembly Text) file for inspection
#
# Usage:
#   ./build.sh          # Build and copy WASM
#   ./build.sh --wat    # Build, copy WASM, and generate WAT

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}Building note-spend-guest...${NC}"

# Ensure the wasm32-wasip1 target is installed
echo "Checking for wasm32-wasip1 target..."
if ! rustup target list | grep -q "wasm32-wasip1 (installed)"; then
    echo "Installing wasm32-wasip1 target..."
    rustup target add wasm32-wasip1
fi

# Build with optimizations
echo "Building WASM module..."
RUSTFLAGS="-C link-arg=-s" cargo build --target wasm32-wasip1 --release

# Output directory
OUT_DIR="../bins/programs"
mkdir -p "$OUT_DIR"

# Copy the WASM binary
WASM_FILE="target/wasm32-wasip1/release/note_spend_guest.wasm"
cp "$WASM_FILE" "$OUT_DIR/note_spend_guest.wasm"

echo -e "${GREEN}✓ WASM binary built: $OUT_DIR/note_spend_guest.wasm${NC}"

# Get size info
SIZE=$(ls -lh "$OUT_DIR/note_spend_guest.wasm" | awk '{print $5}')
echo "  Size: $SIZE"

# Generate WAT if requested
if [[ "$1" == "--wat" ]] || [[ "$1" == "-w" ]]; then
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

