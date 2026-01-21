# Midnight L2 Service Dashboard

A modern, Midnight-styled React dashboard for monitoring and controlling the Rollup Ligero service stack.

## Features

- Real-time health monitoring of all services (rollup, verifier, fvk-service, indexer, mcp, prover)
- Service control buttons (Start, Stop, Restart, Clean)
- Auto-refresh with 5-second intervals
- Response time / latency tracking
- Midnight Network inspired dark theme

## Prerequisites

- Node.js 18+
- The Rollup Ligero Service Controller running on port 9090

## Quick Start

1. Install dependencies:

```bash
npm install
```

2. Start the development server:

```bash
npm run dev
```

3. Open your browser at [http://localhost:3333](http://localhost:3333)

## Service Controller

The dashboard connects to the service controller at `http://127.0.0.1:9090`. Make sure to start the service controller first:

```bash
# From the rollup-ligero directory
cargo run --bin rollup_ligero_service_controller
```

Or with a custom bind address:

```bash
SERVICE_CONTROLLER_BIND=0.0.0.0:9090 cargo run --bin rollup_ligero_service_controller
```

## API Endpoints

The dashboard communicates with these endpoints:

| Endpoint    | Method     | Description                          |
|-------------|------------|--------------------------------------|
| `/health`   | GET        | Get health status of all services    |
| `/start`    | POST/GET   | Start all services                   |
| `/stop`     | POST/GET   | Stop all services                    |
| `/restart`  | POST/GET   | Restart all services                 |
| `/clean`    | POST/GET   | Clean the demo_data directory        |

## Build for Production

```bash
npm run build
```

The built files will be in the `dist/` directory.

## Configuration

### Environment Variables

Create a `.env` file (or `.env.local`) to configure the dashboard:

| Variable | Default | Description |
|----------|---------|-------------|
| `VITE_API_TARGET` | `http://127.0.0.1:9090` | Service Controller API URL |
| `VITE_PORT` | `3333` | Dev server port |

Example:
```bash
# .env.local
VITE_API_TARGET=http://192.168.1.100:9090
VITE_PORT=8080
```

Or pass inline:
```bash
VITE_API_TARGET=http://remote-server:9090 npm run dev
```

The dashboard uses `/controller/` for API calls, which nginx already proxies to the service controller.

## Monitored Services

| Service     | Default URL                  | Health Path    |
|-------------|------------------------------|----------------|
| rollup      | http://127.0.0.1:12346       | /healthcheck   |
| verifier    | http://127.0.0.1:8080        | /health        |
| fvk-service | http://127.0.0.1:8088        | /health        |
| indexer     | http://127.0.0.1:13100       | /health        |
| mcp         | http://127.0.0.1:3000        | /health        |
| mcp-2       | http://127.0.0.1:3001        | /health        |
| prover      | http://127.0.0.1:1313        | /health        |

## Design

The dashboard uses a design language inspired by [midnight.network](https://midnight.network/):

- Pure black background
- High-contrast white text
- Clean, minimal typography (Inter font)
- Subtle borders and card elevations
- Green/red status indicators with glow effects
- Numbered service cards (01, 02, 03...)
- Uppercase section labels with letter-spacing
