#!/usr/bin/env bash
# Build the note_deposit RISC-V guest and export its ROM as a Rust constant.
#
# Prerequisites:
#   rustup target add riscv32im-unknown-none-elf
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
YELLOW='\033[0;33m'
NC='\033[0m'

# Ensure riscv32im target is installed.
if ! rustup target list --installed | grep -q riscv32im-unknown-none-elf; then
    echo -e "${YELLOW}Installing riscv32im-unknown-none-elf target...${NC}"
    rustup target add riscv32im-unknown-none-elf
fi

EXTRA_ARGS=()
for arg in "$@"; do
    case $arg in
        --skip-build) EXTRA_ARGS+=("--skip-build") ;;
    esac
done

# Build and export.
python3 export_rom_rs.py "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"

echo -e "${GREEN}Done. ROM written to ../note_deposit_rom.rs${NC}"
