# Rollup Ligero Services (Linux)

This directory contains systemd unit files for running the Sovereign SDK
rollup demo and its Ligero proof verifier as background services on Linux.

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
   ```
2. Reload systemd so it detects the new files:
   ```bash
   sudo systemctl daemon-reload
   ```
3. Enable and start the services:
   ```bash
   sudo systemctl enable --now rollup-ligero.service
   sudo systemctl enable --now rollup-ligero-verifier.service
   ```

## Useful Commands
- Check status: `systemctl status rollup-ligero.service`
- View logs: `journalctl -u rollup-ligero.service -f`
- Restart after code changes: `sudo systemctl restart rollup-ligero.service`
  (repeat for `rollup-ligero-verifier.service` as needed)

## Notes
- The verifier service wraps `run_verifier_service.sh`, which builds and runs
  `sov-proof-verifier-service`. Adjust environment variables inside the script
  (or in the unit file) if you need non-default Ligero settings.
- Ensure that both services run under a user with permission to access the
  workspace and required key material.

