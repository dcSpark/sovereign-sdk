#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'EOF'
Derive wallet and privacy addresses from private key material.

Usage:
  scripts/derive_wallet_addresses.sh <wallet_private_key_hex> [privacy_spend_key_hex]
  scripts/derive_wallet_addresses.sh --wallet-private-key <wallet_private_key_hex> [--privacy-spend-key <privacy_spend_key_hex>]

Notes:
  - Keys must be 32-byte hex (with or without 0x prefix).
  - If privacy_spend_key_hex is omitted, wallet_private_key_hex is reused.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || "$#" -eq 0 ]]; then
  usage
  exit 0
fi

if [[ "${1:-}" == "--wallet-private-key" || "${1:-}" == "--privacy-spend-key" ]]; then
  cargo run --quiet -p mcp-external --bin derive_wallet_addresses -- "$@"
  exit 0
fi

wallet_key="$1"
privacy_key="${2:-}"

if [[ -n "$privacy_key" ]]; then
  cargo run --quiet -p mcp-external --bin derive_wallet_addresses -- \
    --wallet-private-key "$wallet_key" \
    --privacy-spend-key "$privacy_key"
else
  cargo run --quiet -p mcp-external --bin derive_wallet_addresses -- \
    --wallet-private-key "$wallet_key"
fi
