# Rollup Ligero Services (Linux)

This directory contains systemd unit files for running the Sovereign SDK
rollup demo, its Ligero proof verifier, the Ligero prover service, the indexer,
the MCP external server, and the continuous transfers load generator as
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
   sudo cp rollup-ligero-prover.service /etc/systemd/system/
   sudo cp rollup-ligero-indexer.service /etc/systemd/system/
   sudo cp rollup-ligero-continuous-transfers.service /etc/systemd/system/
   sudo cp rollup-ligero-mcp.service /etc/systemd/system/
   ```
2. Reload systemd so it detects the new files:
   ```bash
   sudo systemctl daemon-reload
   ```
3. Enable and start the services:
   ```bash
   sudo systemctl enable --now rollup-ligero.service
   sudo systemctl enable --now rollup-ligero-verifier.service
   sudo systemctl enable --now rollup-ligero-prover.service
   sudo systemctl enable --now rollup-ligero-indexer.service
   sudo systemctl enable --now rollup-ligero-continuous-transfers.service
   sudo systemctl enable --now rollup-ligero-mcp.service
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
- The prover service wraps `run_prover.sh`, which builds and runs the
  `ligero-http-server` binary from the ligero-prover git dependency. It provides
  HTTP endpoints for ZK proof generation (`POST /prove`) and verification
  (`POST /verify`). Configure via environment variables:
  - `PROVER_BIND_ADDR`: Bind address (default: `0.0.0.0:1313`)
  - `PROVER_THREADS`: Number of HTTP worker threads (default: CPU count)
  - `PROVER_PROOF_OUTPUTS`: Directory for proof outputs
  - `PROVER_KEEP_PROOF_DIRS`: Set to `1` to keep proof directories for debugging
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
- Ensure that all services run under a user with permission to access the
  workspace and required key material.
