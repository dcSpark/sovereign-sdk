#!/bin/bash

# Launch script for Grafana + Prometheus + Telegraf monitoring stack

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"

echo "🚀 Launching monitoring stack..."
echo ""

# Check if docker-compose is available
if ! command -v docker-compose &> /dev/null; then
    if ! command -v docker &> /dev/null; then
        echo "❌ Error: Docker is not installed"
        exit 1
    fi

    # Try using 'docker compose' (newer syntax)
    if docker compose version &> /dev/null; then
        COMPOSE_CMD="docker compose"
    else
        echo "❌ Error: docker-compose is not installed"
        echo "Please install docker-compose: https://docs.docker.com/compose/install/"
        exit 1
    fi
else
    COMPOSE_CMD="docker-compose"
fi

# Start the stack
echo "📦 Starting containers..."
$COMPOSE_CMD -f "$COMPOSE_FILE" up -d

echo ""
echo "✅ Monitoring stack is running!"
echo ""
echo "📊 Access points:"
echo "   - Grafana:    http://localhost:3000 (admin/admin)"
echo "   - Prometheus: http://localhost:9090"
echo "   - Telegraf:   http://localhost:9273/metrics"
echo ""
echo "📈 Dashboard:"
echo "   1. Open Grafana at http://localhost:3000"
echo "   2. Login with admin/admin"
echo "   3. Go to Dashboards → Browse → Sequencer → Parallel Executor Performance"
echo ""
echo "🔍 Checking status..."
sleep 3

# Check if containers are running
if $COMPOSE_CMD -f "$COMPOSE_FILE" ps | grep -q "Up"; then
    echo "✅ All containers are running"
    echo ""
    $COMPOSE_CMD -f "$COMPOSE_FILE" ps
else
    echo "⚠️  Some containers may not be running"
    $COMPOSE_CMD -f "$COMPOSE_FILE" ps
fi

echo ""
echo "📝 Useful commands:"
echo "   - View logs:        $COMPOSE_CMD -f $COMPOSE_FILE logs -f"
echo "   - Stop stack:       $COMPOSE_CMD -f $COMPOSE_FILE down"
echo "   - Restart:          $COMPOSE_CMD -f $COMPOSE_FILE restart"
echo "   - View this script: cat $0"
echo ""
echo "📖 Full documentation: $SCRIPT_DIR/GRAFANA_SETUP.md"
