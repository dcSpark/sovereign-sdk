#!/bin/bash
# Build script for Rust guest program (value_validator_rust.wasm)
#
# This compiles a Rust program to WebAssembly for use with Ligero.
# This is a proof-of-concept to verify Rust->WASM works with Ligero.

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}Building Rust Guest Program (value_validator_rust.wasm)${NC}"
echo ""

# Check for rustc
if ! command -v rustc &> /dev/null; then
    echo -e "${RED}Error: Rust not found!${NC}"
    echo "Please install Rust: https://rustup.rs/"
    exit 1
fi

# Check for wasm32-wasip1 target (newer Rust versions) or wasm32-wasi (older)
WASM_TARGET="wasm32-wasip1"
if ! rustup target list --installed | grep -q "$WASM_TARGET"; then
    # Try old name if new one doesn't exist
    if rustup target list | grep -q "wasm32-wasi" && ! rustup target list | grep -q "$WASM_TARGET"; then
        WASM_TARGET="wasm32-wasi"
    fi
    echo -e "${YELLOW}Installing $WASM_TARGET target...${NC}"
    rustup target add $WASM_TARGET
fi

# Build the WASM
echo -e "${YELLOW}Compiling to WASM (target: $WASM_TARGET)...${NC}"
cargo build --target $WASM_TARGET --release

# Check output
if [ -f "target/$WASM_TARGET/release/value_validator_rust.wasm" ]; then
    echo ""
    echo -e "${GREEN}✓ Build successful!${NC}"
    echo "Output: $(pwd)/target/$WASM_TARGET/release/value_validator_rust.wasm"
    echo "Size: $(ls -lh target/$WASM_TARGET/release/value_validator_rust.wasm | awk '{print $5}')"
    
    # Convert to WAT for debugging
    if command -v wasm2wat &> /dev/null; then
        echo -e "${YELLOW}Converting to WAT format for debugging...${NC}"
        wasm2wat target/$WASM_TARGET/release/value_validator_rust.wasm \
                 -o target/$WASM_TARGET/release/value_validator_rust.wat
        echo "✓ WAT file: $(pwd)/target/$WASM_TARGET/release/value_validator_rust.wat"
    else
        echo -e "${YELLOW}Note: wasm2wat not found - install wabt to generate WAT files${NC}"
    fi
    
    # Copy to bins/programs (or prompt if interactive)
    if [ -t 0 ]; then
        # Running interactively
        echo ""
        read -p "Copy to ../bins/programs/? [y/N] " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            mkdir -p ../bins/programs
            cp target/$WASM_TARGET/release/value_validator_rust.wasm ../bins/programs/
            echo -e "${GREEN}✓ Copied to ../bins/programs/value_validator_rust.wasm${NC}"
            
            # Also copy WAT file if it exists
            if [ -f "target/$WASM_TARGET/release/value_validator_rust.wat" ]; then
                cp target/$WASM_TARGET/release/value_validator_rust.wat ../bins/programs/
                echo -e "${GREEN}✓ Copied to ../bins/programs/value_validator_rust.wat${NC}"
            fi
        fi
    else
        # Running non-interactively - auto-copy
        echo "Auto-copying to ../bins/programs/"
        mkdir -p ../bins/programs
        cp target/$WASM_TARGET/release/value_validator_rust.wasm ../bins/programs/
        echo -e "${GREEN}✓ Copied to ../bins/programs/value_validator_rust.wasm${NC}"
        
        # Also copy WAT file if it exists
        if [ -f "target/$WASM_TARGET/release/value_validator_rust.wat" ]; then
            cp target/$WASM_TARGET/release/value_validator_rust.wat ../bins/programs/
            echo -e "${GREEN}✓ Copied to ../bins/programs/value_validator_rust.wat${NC}"
        fi
    fi
else
    echo -e "${RED}✗ Build failed - output file not found${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}Next steps:${NC}"
echo "1. Test with Ligero prover:"
echo "   cd ../bins"
echo "   ./webgpu_prover <config.json>"
echo ""
echo "2. Compare with C++ version to ensure same behavior"

