# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "🚀 Starting ligero indexer..."
# Run the ligero indexer from examples/rollup-ligero directory
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

export RUST_LOG="${RUST_LOG:-info}"
export DA_CONNECTION_STRING="sqlite://demo_data/da.sqlite?mode=rwc"

cargo run -p sov-indexer --release