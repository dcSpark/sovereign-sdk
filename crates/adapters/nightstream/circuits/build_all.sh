#!/usr/bin/env bash
# Build all Nightstream circuit guests and export their checked-in byte arrays.
#
# This script iterates over each circuit subdirectory containing a build.sh
# and runs it. The generated *_rom.rs files are written to this directory.
#
# Prerequisites:
#   rustup toolchain install nightly
#   python3
#
# The Nightstream repo must be a sibling of the sovereign-ligero checkout,
# or set NIGHTSTREAM_ROOT to the Nightstream repo root.
#
# Usage:
#   ./build_all.sh              # build + export all circuits
#   ./build_all.sh --skip-build # export only (re-run on existing ELFs)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

FAILED=0

for circuit_dir in */; do
    # Skip directories without a build.sh.
    if [ ! -f "$circuit_dir/build.sh" ]; then
        continue
    fi

    circuit_name="${circuit_dir%/}"
    echo -e "${BLUE}=== Building circuit: ${circuit_name} ===${NC}"

    if bash "$circuit_dir/build.sh" "$@"; then
        echo -e "${GREEN}=== ${circuit_name}: OK ===${NC}"
    else
        echo -e "${RED}=== ${circuit_name}: FAILED ===${NC}"
        FAILED=1
    fi
    echo
done

if [ "$FAILED" -ne 0 ]; then
    echo -e "${RED}Some circuits failed to build.${NC}"
    exit 1
fi

echo -e "${GREEN}All circuits built successfully.${NC}"
