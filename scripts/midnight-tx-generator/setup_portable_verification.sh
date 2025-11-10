#!/bin/bash
# Copy required files for portable verification testing
# This simulates what you'd do on a remote verifier machine

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== Setting up portable verification ===${NC}\n"

# Check if portable config exists
if [ ! -f "$SCRIPT_DIR/verify_config.json" ]; then
    echo -e "${RED}Error: verify_config.json not found${NC}"
    echo "Run ./generate_verify_command.sh first"
    exit 1
fi

# Check if it's a portable config (uses relative paths)
if grep -q "note_spend_guest.wasm" "$SCRIPT_DIR/verify_config.json" 2>/dev/null; then
    echo -e "${GREEN}✓ Found portable config${NC}"
else
    echo -e "${YELLOW}Warning: Config appears to use absolute paths${NC}"
    echo "This script is for portable mode. Run ./generate_verify_command.sh without --local"
fi

# Determine platform
if [[ "$OSTYPE" == "darwin"* ]]; then
    PLATFORM="macos"
else
    PLATFORM="linux-amd64"
fi

echo -e "\n${BLUE}Copying required files...${NC}"

# Copy WASM program
if [ -f "$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm" ]; then
    cp "$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm" "$SCRIPT_DIR/"
    echo -e "  ${GREEN}✓${NC} note_spend_guest.wasm"
else
    echo -e "  ${RED}✗${NC} note_spend_guest.wasm not found"
    exit 1
fi

# Copy shader directory
if [ -d "$REPO_ROOT/crates/adapters/ligero/bins/$PLATFORM/shader" ]; then
    cp -r "$REPO_ROOT/crates/adapters/ligero/bins/$PLATFORM/shader" "$SCRIPT_DIR/"
    echo -e "  ${GREEN}✓${NC} shader/ directory"
else
    echo -e "  ${RED}✗${NC} shader/ directory not found"
    exit 1
fi

echo -e "\n${GREEN}=== Setup complete! ===${NC}"
echo -e "\n${BLUE}Files ready for portable verification:${NC}"
ls -lh "$SCRIPT_DIR" | grep -E "(proof_data.gz|verify_config.json|verify_command.sh|note_spend_guest.wasm|shader)" || true
echo ""

echo -e "${BLUE}Test locally:${NC}"
echo "  WEBGPU_VERIFIER=../../crates/adapters/ligero/bins/$PLATFORM/bin/webgpu_verifier ./verify_command.sh"
echo ""

echo -e "${BLUE}Or run directly:${NC}"
echo "  WEBGPU_VERIFIER=../../crates/adapters/ligero/bins/$PLATFORM/bin/webgpu_verifier"
echo "  \$WEBGPU_VERIFIER \"\$(cat verify_config.json)\""
echo ""

echo -e "${GREEN}Files to copy to remote verifier:${NC}"
echo "  • proof_data.gz"
echo "  • verify_config.json  "
echo "  • note_spend_guest.wasm"
echo "  • shader/ (directory)"
echo "  • webgpu_verifier (from crates/adapters/ligero/bins/$PLATFORM/bin/)"
echo ""
echo -e "${YELLOW}Note: webgpu_verifier has library dependencies. On the remote machine,"
echo "        keep it in its original directory structure with ../lib/ available.${NC}"

