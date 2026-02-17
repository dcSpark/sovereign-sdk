# Midnight L2 Service Dashboard

A modern, Midnight-styled React dashboard for monitoring and controlling the Rollup Nightstream service stack.

## Features

- Real-time health monitoring of all services (rollup, worker, fvk, indexer, mcp, metrics, oracle)
- Global service controls (Start All, Stop All, Restart All, Clean, Clean Database, Reset TEE)
- Per-service controls directly in the services table (Start/Stop/Restart by service id)
- Auto-refresh with 5-second intervals
- Response time / latency tracking
- Midnight Network inspired dark theme

## Prerequisites

- Node.js 18+
- The Rollup Nightstream Service Controller running on port 9090

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
# From the rollup-nightstream directory
cargo run --bin rollup-nightstream-service-controller
```

Or with a custom bind address:

```bash
SERVICE_CONTROLLER_BIND=0.0.0.0:9090 cargo run --bin rollup-nightstream-service-controller
```

## API Endpoints

The dashboard communicates with these endpoints:

| Endpoint    | Method     | Description                          |
|-------------|------------|--------------------------------------|
| `/health`   | GET        | Get health status of all services    |
| `/services` | GET        | Get controller-managed process state |
| `/start`    | POST/GET   | Start default service set            |
| `/stop`     | POST/GET   | Stop all running services            |
| `/restart`  | POST/GET   | Restart default service set          |
| `/clean-database`| POST/GET   | Drop all tables with CASCADE in `da`, `indexer`, `fvk`, `mcp_sessions` |
| `/reset-tee`| POST/GET   | Trigger TEE reset via configured upstream URL |
| `/start/:service`   | POST/GET | Start one service (`rollup`, `worker`, `indexer`, `mcp`, `metrics`, `oracle`, `fvk`) |
| `/stop/:service`    | POST/GET | Stop one service |
| `/restart/:service` | POST/GET | Restart one service |
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
| worker      | http://127.0.0.1:8080        | /health        |
| fvk         | http://127.0.0.1:8088        | /health        |
| indexer     | http://127.0.0.1:13100       | /health        |
| mcp         | http://127.0.0.1:3000        | /health        |
| metrics     | http://127.0.0.1:13200       | /health        |
| oracle      | http://127.0.0.1:8090        | /              |

### Remote services

To show a service as remote (no local Start/Stop/Restart actions), set:

```bash
SERVICE_WORKER_REMOTE=1
SERVICE_WORKER_URL=http://10.0.0.42:8080
```

The dashboard will display `Process = remote`, disable actions for that service, and use the configured remote URL as endpoint.

### Reset TEE configuration

The `Reset TEE` button calls `POST /controller/reset-tee`. Configure the upstream target and bearer token on the service controller process:

```bash
TEE_RESET_URL=http://74.235.106.62:9898/reset
TEE_RESET_BEARER_TOKEN=replace_with_real_token
```

Success is treated as HTTP `204 No Content` from the upstream service.
This action is allowed only when all managed services are stopped (same precondition as `Clean Data`).

### Clean Database configuration

The `Clean Database` button calls `POST /controller/clean-database` on the service controller.  
It requires `DA_CONNECTION_STRING` to be set on the service controller process (must be PostgreSQL).
This action is allowed only when all managed services are stopped (same precondition as `Clean Data`).

## Design

The dashboard uses a design language inspired by [midnight.network](https://midnight.network/):

- Pure black background
- High-contrast white text
- Clean, minimal typography (Inter font)
- Subtle borders and card elevations
- Green/red status indicators with glow effects
- Numbered service cards (01, 02, 03...)
- Uppercase section labels with letter-spacing
