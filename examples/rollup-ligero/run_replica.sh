#!/bin/bash
set -e

# Minimal script to run a read-only replica node
# Syncs from the shared DA database but does not write to it

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Ligero configuration
export LIGERO_PACKING=8192

# Default log level
export RUST_LOG="${RUST_LOG:-info}"

echo "Building replica rollup..."
cd "$WORKSPACE_ROOT"
cargo build --release -p sov-rollup-ligero

echo ""
echo "Starting replica node (read-only mode)..."
echo "  Config: rollup_config_replica.toml"
echo "  API Port: 12347"
echo "  Prometheus Port: 13201 (primary uses 13200)"
echo "  Storage: demo_data_replica/"
echo ""

cd "$WORKSPACE_ROOT/examples/rollup-ligero"

# Create replica data directory
mkdir -p demo_data_replica

# Use different Prometheus port than primary (13200) to allow running both on same machine
exec "$WORKSPACE_ROOT/target/release/sov-rollup-ligero" \
    --rollup-config-path rollup_config_replica.toml \
    --prometheus-exporter-bind "0.0.0.0:13201" \
    "$@"
