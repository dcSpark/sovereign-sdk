#!/bin/bash
# Build script for value_validator Ligero guest program
#
# Prerequisites:
#   1. Emscripten SDK installed and activated
#   2. Ligero SDK built (ligero-vm/ligero-prover/sdk/build/libligetron.a)
#
# Usage:
#   ./build.sh

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}Building Ligero Value Validator Guest Program${NC}"
echo ""

# Check for emscripten
if ! command -v emcc &> /dev/null; then
    echo -e "${RED}Error: Emscripten not found!${NC}"
    echo "Please install and activate Emscripten:"
    echo "  cd /path/to/emsdk"
    echo "  source emsdk_env.sh"
    exit 1
fi

# Check for Ligero SDK
LIGERO_SDK_PATH="${LIGERO_SDK_PATH:-../../../../../ligero-vm/ligero-prover/sdk}"
if [ ! -f "$LIGERO_SDK_PATH/build/libligetron.a" ]; then
    echo -e "${RED}Error: Ligero SDK not found or not built!${NC}"
    echo "Expected: $LIGERO_SDK_PATH/build/libligetron.a"
    echo ""
    echo "Please build the Ligero SDK first:"
    echo "  cd ligero-vm/ligero-prover/sdk"
    echo "  mkdir -p build && cd build"
    echo "  emcmake cmake .."
    echo "  emmake make"
    exit 1
fi

# Create build directory
echo -e "${YELLOW}Creating build directory...${NC}"
mkdir -p build
cd build

# Configure with CMake
echo -e "${YELLOW}Configuring with CMake...${NC}"
emcmake cmake .. -DLIGERO_SDK_PATH="$LIGERO_SDK_PATH"

# Build
echo -e "${YELLOW}Building...${NC}"
emmake make

# Check output
if [ -f "value_validator.wasm" ]; then
    echo ""
    echo -e "${GREEN}✓ Build successful!${NC}"
    echo "Output: $(pwd)/value_validator.wasm"
    echo "Size: $(ls -lh value_validator.wasm | awk '{print $5}')"
    
    # Optionally copy to bins/programs
    echo ""
    read -p "Copy to ../bins/programs/? [y/N] " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        mkdir -p ../bins/programs
        cp value_validator.wasm ../bins/programs/
        echo -e "${GREEN}✓ Copied to ../bins/programs/value_validator.wasm${NC}"
    fi
else
    echo -e "${RED}✗ Build failed - output file not found${NC}"
    exit 1
fi

