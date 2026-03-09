#!/usr/bin/env bash
# Build the value_validator RV64IM guest and export its checked-in bytes as a Rust constant.
#
# Prerequisites:
#   rustup toolchain install nightly
#
# The nightstream-sdk dependency is fetched from GitHub automatically via Cargo.
#
# Usage:
#   ./build.sh              # build + export ROM
#   ./build.sh --skip-build # export only (re-run export_rom_rs.py on existing ELF)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

GREEN='\033[0;32m'
NC='\033[0m'

EXTRA_ARGS=()
for arg in "$@"; do
    case $arg in
        --skip-build) EXTRA_ARGS+=("--skip-build") ;;
    esac
done

# Build and export.
python3 export_rom_rs.py "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"

echo -e "${GREEN}Done. Guest bytes written to ../value_validator_rom.rs${NC}"
