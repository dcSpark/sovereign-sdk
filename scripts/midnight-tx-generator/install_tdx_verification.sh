#!/bin/bash
# Install TDX Quote Verification Tools
#
# This script installs Google's go-tdx-guest tooling for verifying TDX quotes.
# Run this on the machine where you'll verify attestations (can be outside TEE).

set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== Installing TDX Quote Verification Tools ===${NC}\n"

# Check if Go is installed
if ! command -v go &> /dev/null; then
    echo -e "${RED}Error: Go is not installed${NC}"
    echo "Install Go from: https://go.dev/doc/install"
    echo ""
    echo "Quick install (Linux):"
    echo "  wget https://go.dev/dl/go1.21.5.linux-amd64.tar.gz"
    echo "  sudo rm -rf /usr/local/go"
    echo "  sudo tar -C /usr/local -xzf go1.21.5.linux-amd64.tar.gz"
    echo "  export PATH=\$PATH:/usr/local/go/bin"
    exit 1
fi

echo -e "${GREEN}✓${NC} Go is installed: $(go version)"
echo ""

# Install go-tdx-guest
echo -e "${YELLOW}Installing go-tdx-guest...${NC}"
go install github.com/google/go-tdx-guest/tools/check@latest

# Verify installation
if [ -f "$HOME/go/bin/check" ]; then
    echo -e "${GREEN}✓${NC} Installed successfully: $HOME/go/bin/check"
    echo ""
    echo -e "${BLUE}Add to PATH:${NC}"
    echo "  export PATH=\"\$PATH:\$HOME/go/bin\""
    echo "  echo 'export PATH=\"\$PATH:\$HOME/go/bin\"' >> ~/.bashrc"
    echo ""
    echo -e "${BLUE}Test verification:${NC}"
    echo "  check -in quote.dat"
else
    echo -e "${RED}✗${NC} Installation failed"
    exit 1
fi

echo ""
echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
echo -e "${GREEN}✓ Installation Complete${NC}"
echo -e "${GREEN}═══════════════════════════════════════════════${NC}"

