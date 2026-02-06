# Rollup Ligero Services (Linux)

This directory contains systemd unit files for running the Sovereign SDK
rollup demo, its Ligero proof verifier, the indexer, the MCP external server,
the service controller, and the continuous transfers load generator as
background services on Linux.

## Prerequisites
- A Linux host with `systemd`
- Rust toolchain and build dependencies for the project
- Built Ligero guest programs (from the Ligero repo): `<ligero-prover>/utils/circuits/bins/*.wasm`
- Configured indexer environment at
  `crates/utils/sov-indexer/.env` (`DA_CONNECTION_STRING`, `INDEX_DB`,
  `INDEXER_BIND`, optional VFK settings)
- Updated unit files with the correct `User`, `WorkingDirectory`, `ExecStart`,
  and `PATH` values for your environment

## Installation
1. Copy the unit files into `/etc/systemd/system/`:
   ```bash
   sudo cp rollup-ligero.service /etc/systemd/system/
   sudo cp rollup-ligero-verifier.service /etc/systemd/system/
   sudo cp rollup-ligero-indexer.service /etc/systemd/system/
   sudo cp rollup-ligero-continuous-transfers.service /etc/systemd/system/
   sudo cp rollup-ligero-mcp.service /etc/systemd/system/
   sudo cp rollup-ligero-service-controller.service /etc/systemd/system/
   ```
2. Reload systemd so it detects the new files:
   ```bash
   sudo systemctl daemon-reload
   ```
3. Enable and start the services:
   ```bash
   sudo systemctl enable --now rollup-ligero.service
   sudo systemctl enable --now rollup-ligero-verifier.service
   sudo systemctl enable --now rollup-ligero-indexer.service
   sudo systemctl enable --now rollup-ligero-continuous-transfers.service
   sudo systemctl enable --now rollup-ligero-mcp.service
   sudo systemctl enable --now rollup-ligero-service-controller.service
   ```

## Useful Commands
- Check status: `systemctl status rollup-ligero.service` (swap in the service
  name you want to inspect, e.g. `rollup-ligero-indexer.service`)
- View logs: `journalctl -u rollup-ligero.service -f`
- Restart after code changes: `sudo systemctl restart rollup-ligero.service`
  (repeat for `rollup-ligero-verifier.service`,
  `rollup-ligero-indexer.service`, and
  `rollup-ligero-continuous-transfers.service` as needed)

## Notes
- The verifier service wraps `run_verifier_service.sh`, which builds and runs
  `sov-proof-verifier-service`. Adjust environment variables inside the script
  (or in the unit file) if you need non-default Ligero settings.
- The indexer service runs `cargo run -p sov-indexer --release` from
  `crates/utils/sov-indexer` and loads `.env` via `EnvironmentFile=`. Ensure
  `DA_CONNECTION_STRING` points to your rollup DA SQLite DB
  (e.g. `examples/rollup-ligero/demo_data/da.sqlite?mode=ro`) and set any FVK
  config you need.
- The continuous transfers service runs
  `cargo run -p midnight-e2e-benchmarks --bin continuous_transfers --release`.
  Update the `Environment=` entries inside the unit file to control the number
  of wallets (`CONTINUOUS_NUM_WALLETS`) and the verifier/sequencer endpoints
  (`E2E_ROLLUP_EXTERNAL_VERIFIER_URL` and `E2E_ROLLUP_EXTERNAL_NODE_URL`).
- The MCP external service wraps `crates/mcp-external/run_mcp.sh`. It loads
  environment from `crates/mcp-external/.env` (or inline `Environment=` overrides)
  for MCP address, rollup/verifier/indexer endpoints, keys, and Ligero paths.
- The service controller runs
  `cargo run -p sov-rollup-ligero --bin rollup-ligero-service-controller --release`.
  It provides HTTP endpoints for managing all other services:
  - `POST /start`: Start all services via `run_all.sh`
  - `POST /stop`: Stop all running services
  - `POST /restart`: Restart all services
  - `POST /clean`: Remove the `demo_data` directory
  - `POST /clean-database`: Drop all tables with `CASCADE` from `da`, `indexer`, `fvk`, `mcp_sessions`
  - `POST /reset-tee`: Trigger TEE reset on the configured upstream endpoint
  - `GET /health`: Check health of all services (rollup, worker, indexer, mcp, fvk)
  - Note: `clean`, `clean-database`, and `reset-tee` require all managed services to be stopped
  Configure via environment variables:
  - `SERVICE_CONTROLLER_BIND`: Bind address (default: `127.0.0.1:9090`)
  - `SERVICE_CONTROLLER_AUTO_START`: Set to `1` to auto-start all services when the controller starts
  - `DA_CONNECTION_STRING`: PostgreSQL base connection string used by `/clean-database`
  - `TEE_RESET_URL`: TEE reset endpoint URL (default: `http://74.235.106.62:9898/reset`)
  - `TEE_RESET_BEARER_TOKEN`: Bearer token for `TEE_RESET_URL` (`TEE_RESET_TOKEN` alias also supported)
  - `SERVICE_<SERVICE>_REMOTE`: Set to `1` to mark a service as remote (example: `SERVICE_WORKER_REMOTE=1`)
  - `SERVICE_<SERVICE>_URL`: Override health endpoint URL for that service (example: `SERVICE_WORKER_URL=http://remote-host:8080`)
- Ensure that all services run under a user with permission to access the
  workspace and required key material.
