# Rollup Ligero Services (Linux)

This directory contains systemd unit files for running the Sovereign SDK
rollup demo, its Ligero proof verifier, and the continuous transfers load
generator as background services on Linux.

## Prerequisites
- A Linux host with `systemd`
- Rust toolchain and build dependencies for the project
- Built Ligero assets (`crates/adapters/ligero/guest && ./build.sh`)
- Updated unit files with the correct `User`, `WorkingDirectory`, `ExecStart`,
  and `PATH` values for your environment

## Installation
1. Copy the unit files into `/etc/systemd/system/`:
   ```bash
   sudo cp rollup-ligero.service /etc/systemd/system/
   sudo cp rollup-ligero-verifier.service /etc/systemd/system/
   sudo cp rollup-ligero-continuous-transfers.service /etc/systemd/system/
   ```
2. Reload systemd so it detects the new files:
   ```bash
   sudo systemctl daemon-reload
   ```
3. Enable and start the services:
   ```bash
   sudo systemctl enable --now rollup-ligero.service
   sudo systemctl enable --now rollup-ligero-verifier.service
   sudo systemctl enable --now rollup-ligero-continuous-transfers.service
   ```

## Useful Commands
- Check status: `systemctl status rollup-ligero.service`
- View logs: `journalctl -u rollup-ligero.service -f`
- Restart after code changes: `sudo systemctl restart rollup-ligero.service`
  (repeat for `rollup-ligero-verifier.service` and
  `rollup-ligero-continuous-transfers.service` as needed)

## Notes
- The verifier service wraps `run_verifier_service.sh`, which builds and runs
  `sov-proof-verifier-service`. Adjust environment variables inside the script
  (or in the unit file) if you need non-default Ligero settings.
- The continuous transfers service runs
  `cargo run -p sov-rollup-ligero --bin continuous-transfers --release`. Update
  the `Environment=` entries inside the unit file to control the number of
  wallets (`CONTINUOUS_NUM_WALLETS`) and the verifier/sequencer endpoints
  (`E2E_ROLLUP_EXTERNAL_VERIFIER_URL` and `E2E_ROLLUP_EXTERNAL_NODE_URL`).
- Ensure that all services run under a user with permission to access the
  workspace and required key material.

