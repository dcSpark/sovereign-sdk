#!/bin/bash
# Script to generate deterministic genesis keys and bank.json file

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "======================================"
echo "Generating Genesis Keys"
echo "======================================"
echo ""

# Run the generate-genesis-keys binary
cargo run --bin generate-genesis-keys

echo ""
echo "======================================"
echo "Genesis Generation Complete!"
echo "======================================"
echo ""
echo "Generated files:"
echo "  - ../test-data/genesis/demo/mock/bank.json (1000 funded accounts)"
echo "  - ../test-data/genesis/demo/mock/generated_keypairs.json (private keys)"
echo ""
echo "⚠️  IMPORTANT: Keep generated_keypairs.json secure!"
echo "    It contains private keys for all 1000 genesis accounts."
echo ""
echo "Next steps:"
echo "  1. Start your rollup node (it will use the new genesis file)"
echo "  2. Run e2e tests with up to 1000 parallel accounts"
echo ""

