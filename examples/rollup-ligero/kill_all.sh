#!/usr/bin/env bash
# Kill all rollup-ligero related processes

set -euo pipefail

echo "Looking for rollup-ligero related processes..."

# Ports used by run_all.sh services
PORTS=(12346 8080 9090 3000 3001 13100)

# Process patterns to kill
PATTERNS=(
    "sov-rollup-ligero"
    "proof-verifier"
    "sov-indexer"
    "mcp-external"
)

killed_any=false

# Kill by process name patterns
for pattern in "${PATTERNS[@]}"; do
    pids=$(pgrep -f "$pattern" 2>/dev/null || true)
    if [ -n "$pids" ]; then
        echo "Killing processes matching '$pattern': $pids"
        echo "$pids" | xargs kill -9 2>/dev/null || true
        killed_any=true
    fi
done

# Kill by port (in case something else is blocking)
for port in "${PORTS[@]}"; do
    pid=$(lsof -ti ":$port" 2>/dev/null || true)
    if [ -n "$pid" ]; then
        echo "Killing process on port $port: $pid"
        echo "$pid" | xargs kill -9 2>/dev/null || true
        killed_any=true
    fi
done

if [ "$killed_any" = true ]; then
    echo ""
    echo "✓ Killed blocking processes. You can now run ./run_all.sh"
else
    echo "✓ No blocking processes found. All clear!"
fi
