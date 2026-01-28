# Get the workspace root (assuming this script is in examples/rollup-ligero/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "🚀 Starting Oracle..."
cd "$WORKSPACE_ROOT/examples/rollup-ligero"

cargo run -p oracle --release
