# Nightstream Rollup

A Sovereign SDK rollup using **Midnight DA** with **MockZkvm** for both inner and outer VMs. This is the Nightstream variant that replaces the Ligero zkVM with native Rust proof verification via the Nightstream adapter.

## Overview

This rollup includes:

- **Midnight DA Layer**: PostgreSQL-backed data availability using `StorableMidnightDaService`
- **MockZkvm**: For both inner and outer VM (no external prover binaries needed)
- **Midnight Bridge**: Optional background task that polls for L1 deposits and mints tokens on the rollup
- **All standard modules**: Bank, EVM, Sequencer, Privacy, etc.
- **Service Controller**: Unified process manager for all rollup services

## Prerequisites

- Rust toolchain (stable)
- PostgreSQL (local or remote)
- Node.js 18+ (for the dashboard, optional)

### Database Setup

Create the required PostgreSQL databases:

```bash
createdb da
createdb indexer
createdb fvk
```

Default connection: `postgresql://admin:1234@localhost:5432/da`

## Quick Start

### 1. Start all services with the controller

```bash
./run_controller.sh
```

This builds and launches all services (rollup, worker/verifier, indexer, metrics, oracle, FVK) through a unified service controller with a REST API.

Options:
- `--skip-build` -- Reuse existing binaries
- `--debug` -- Build in debug mode (default is release)
- `--no-auto-start` -- Don't auto-start services on launch
- `--bind <ADDR>` -- Controller bind address (default: `127.0.0.1:9090`)

### 2. Start individual services

```bash
./run_rollup.sh                # Rollup node only
./run_verifier_service.sh      # Proof verifier service
./run_indexer.sh               # Indexer
./run_oracle.sh                # TEE oracle
./run_fvk_service.sh           # FVK service
./run_metrics.sh               # Metrics API
./run_proof_pool.sh            # Proof pool (disabled by default)
./run_mcp.sh                   # MCP external (disabled by default)
```

## Binaries

| Binary | Description |
|--------|-------------|
| `sov-rollup-nightstream` | Main rollup node |
| `rollup-nightstream-service-controller` | Manages all child services via REST API |
| `generate-genesis-keys` | Generates deterministic genesis keys and bank.json |
| `generate-authority-fvk` | Generates Authority Full Viewing Keys for Level-B compliance |
| `decrypt-authority-notes` | Decrypts encrypted notes using an authority FVK |

### Generate Authority FVK

```bash
cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk

# Save to file
cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk -- --output authority_fvk.txt

# Multiple keys in JSON format
cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk -- --format json --count 3
```

### Decrypt Authority Notes

```bash
export AUTHORITY_FVK="0x..."
cargo run --release -p sov-rollup-nightstream --bin decrypt-authority-notes -- --input notes.json
```

### Generate Genesis

```bash
./generate_genesis.sh
```

### Prefund Wallets

```bash
export ADMIN_WALLET_PRIVATE_KEY="<hex>"
./prefund_wallets.sh [output_file] [count] [concurrency]
```

## Configuration

### Main Config: `rollup_config.toml`

Default configuration for local development with PostgreSQL DA.

### Replica Mode: `rollup_config_replica.toml`

Read-only replica that syncs from the shared DA database. Runs on port 12347 to avoid conflicts with the primary node.

```bash
./run_replica.sh
```

### TEE Local Mode: `rollup_config_tee_local.toml`

TEE mode with mock attestation for local development.

```bash
./tee_local.sh
```

Set `TEE_RESET=1` to wipe and regenerate TEE data.

## Service Controller API

When using `run_controller.sh`, a REST API is available:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/services` | GET | List all managed services and their status |
| `/health` | GET | Health check all services |
| `/start` | POST | Start all default services |
| `/start/:service` | POST | Start a specific service |
| `/stop` | POST | Stop all services |
| `/stop/:service` | POST | Stop a specific service |
| `/restart` | POST | Restart all services |
| `/clean` | POST | Clean demo_data directory |
| `/clean-database` | POST | Truncate all Postgres tables |
| `/logs` | GET (WS) | WebSocket log stream |
| `/logs/history` | GET | Recent log history (JSON) |
| `/stats` | GET | System stats (CPU, memory, disk) |

## Dashboard

A React web dashboard is available in `utils/rollup-dashboard/`:

```bash
cd utils/rollup-dashboard
npm install
npm run dev
```

Opens at [http://localhost:3333](http://localhost:3333). See `utils/rollup-dashboard/README.md` for details.

## Midnight Bridge

The bridge polls for L1 deposits and automatically mints tokens on the rollup. It is disabled by default. To enable it, uncomment the `[sequencer.extension.midnight_bridge]` block in `rollup_config.toml`:

```toml
[sequencer.extension.midnight_bridge]
signing_key_path = "demo_data/midnight_bridge_signer.json"
mock_events_path = "demo_data/midnight_bridge_events.json"  # For offline testing
# indexer_http = "https://indexer.preview.midnight.network/api/v3/graphql"
# contract_address = "fa8533250190a9d2b39686523e7b13e7dc30647a341f8163dceaec2cdc365f12"
# indexer_timeout_ms = 30000
poll_interval_ms = 1000
max_fee = 1000000
```

## Environment Variables

Key environment variables (all configurable in `run_controller.sh`):

| Variable | Default | Description |
|----------|---------|-------------|
| `DA_CONNECTION_STRING` | `postgresql://admin:1234@localhost:5432/da` | Shared DA database |
| `ROLLUP_RPC_URL` | `http://127.0.0.1:12346` | Rollup node RPC |
| `BIND_ADDR` | `127.0.0.1:8080` | Worker/verifier bind address |
| `INDEX_DB` | `postgresql://admin:1234@localhost:5432/indexer` | Indexer database |
| `PROOF_POOL_BIND_ADDR` | `127.0.0.1:11235` | Proof pool bind address |
| `MCP_SERVER_BIND_ADDRESS` | `0.0.0.0:3000` | MCP server bind address |
| `METRICS_API_BIND` | `0.0.0.0:13200` | Metrics API bind address |
| `LOG_LEVEL` | `info` | Log level |

## Cleaning Up

```bash
make clean
```

This removes `demo_data/`, old SQLite files, and truncates PostgreSQL tables.

## CVM Policies

TEE attestation policies are in `policies/`:
- `cvm-demo.json` -- Demo environment policy
- `cvm-dev.json` -- Development environment policy

## License

Sovereign Permissionless Commercial License
