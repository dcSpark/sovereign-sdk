# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "🚀 Starting ligero indexer..."
# Run the ligero indexer from examples/rollup-ligero directory
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

export RUST_LOG="${RUST_LOG:-info}"

# DA_CONNECTION_STRING should match the [da].connection_string in rollup_config.toml
# Default to SQLite for local development, but respect environment override for PostgreSQL
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-sqlite://demo_data/da.sqlite?mode=rwc}"

mkdir -p demo_data
export INDEX_DB="${INDEX_DB:-sqlite://demo_data/wallet_index.sqlite?mode=rwc}"

echo "   DA_CONNECTION_STRING=$DA_CONNECTION_STRING"
echo "   INDEX_DB=$INDEX_DB"

cargo run -p sov-indexer --release
