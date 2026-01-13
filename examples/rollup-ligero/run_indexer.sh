echo "🚀 Starting ligero indexer..."
# Run the ligero indexer from examples/rollup-ligero directory
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

export RUST_LOG="${RUST_LOG:-info}"
export DA_CONNECTION_STRING="sqlite://demo_data/da.sqlite?mode=rwc"
cargo run -p sov-indexer --release
