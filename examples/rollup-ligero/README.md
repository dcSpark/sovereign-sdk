# Demo Rollup with Ligero zkVM

This is a variant of the demo-rollup that uses **Ligero** as the zkVM instead of RISC0 or SP1.

## Overview

This rollup demonstrates how to use the Ligero zkVM adapter with Sovereign SDK. It includes:

- **Mock DA Layer**: For local testing
- **Ligero zkVM**: For zero-knowledge proof generation and verification
- **All standard modules**: Bank, EVM, Sequencer, etc.

## Prerequisites

1. **Rust toolchain** (see `rust-toolchain.toml` in the root)
2. **Emscripten SDK** (for building Ligero guest programs)
3. **Ligero SDK** (for proof generation/verification)

### Installing Prerequisites

#### Emscripten SDK
```bash
# Clone and install Emscripten
git clone https://github.com/emscripten-core/emsdk.git
cd emsdk
./emsdk install latest
./emsdk activate latest
source emsdk_env.sh
```

#### Ligero SDK
```bash
# Clone and build Ligero SDK
git clone https://github.com/ligeroinc/ligero-prover.git ligero-vm
cd ligero-vm/ligero-prover/sdk
mkdir -p build && cd build
emcmake cmake ..
emmake make -j
```

## Building

### Skip Guest Build (Recommended for Development)

```bash
export SKIP_GUEST_BUILD=ligero
export SOV_PROVER_MODE=execute  # Fast mode without proving
cargo build --release
```

## Environment Variables for Node Verification

**IMPORTANT**: For the node to verify Ligero proofs from the `value-setter-zk` module, you must set these environment variables:

```bash
# Set Ligero environment variables for proof verification
export LIGERO_PROGRAM_PATH=value_validator_rust
export LIGERO_SHADER_PATH=<ligero-prover>/shader
```

Or simply source the provided script:
```bash
source set_ligero_env.sh
```

These variables tell the verifier where to find:
- **LIGERO_PROGRAM_PATH**: The compiled WASM program used to verify proofs
- **LIGERO_SHADER_PATH**: The GPU shaders for verification

Without these, proof verification will fail with "Code commitment verification failed".

### Full Build (with Guest Programs)

```bash
export LIGERO_SDK_PATH=/path/to/ligero-vm/ligero-prover/sdk
source /path/to/emsdk/emsdk_env.sh
export SOV_PROVER_MODE=prove
cargo build --release
```

## Running

```bash
cd examples/rollup-ligero

# Run with execution mode (no proving)
export SOV_PROVER_MODE=execute
./target/release/sov-rollup-ligero

# Run with full proving
export SOV_PROVER_MODE=prove
./target/release/sov-rollup-ligero
```

## Service Orchestration

### Run All Services Locally

Start the rollup, verifier, indexer, and MCP in order:

```bash
cd examples/rollup-ligero
./run_all.sh
```

Arguments are forwarded to `run_rollup.sh`:

```bash
./run_all.sh -- --stop-at-rollup-height 1300
```

### Rotate Admin Address (Genesis/Config)

The demo setup uses a known admin address. Before production-like deployments, rotate it across genesis/config:

```bash
cd examples/rollup-ligero
./rotate_admin_wallet.sh --new-address <sov1...> --dry-run
./rotate_admin_wallet.sh --new-address <sov1...>
```

Or provide a new admin private key directly (recommended):

```bash
./rotate_admin_wallet.sh --new-key <64-hex-chars> --dry-run
./rotate_admin_wallet.sh --new-key <64-hex-chars> --key-out /secure/path/admin_wallet.json
```

Or generate a fresh admin private key + address automatically:

```bash
./rotate_admin_wallet.sh --dry-run
./rotate_admin_wallet.sh --key-out /secure/path/admin_wallet.json
```

Optional: also update the celestia demo genesis set:

```bash
./rotate_admin_wallet.sh --new-address <sov1...> --include-celestia
```

Important:
- If `--new-key` is used, address is derived from the key and key files are synced.
- If `--new-address` is omitted (and `--new-key` is not set), the script generates a key and prints `GENERATED_ADMIN_WALLET_PRIVATE_KEY=<hex>`.
- In `--new-key` and generated-key modes, it syncs `examples/test-data/keys/token_deployer_private_key.json`.
- In `--new-address` mode, only address references are updated (key files are unchanged).
- Save that key securely and inject it as `ADMIN_WALLET_PRIVATE_KEY` at runtime.
- `run_mcp.sh` and `run_proof_pool.sh` require `ADMIN_WALLET_PRIVATE_KEY` from environment.
- `run_mcp.sh` uses `WALLET_PRIVATE_KEY` when set, otherwise it reuses `ADMIN_WALLET_PRIVATE_KEY`.

### Service Controller API

The controller now manages each service script independently (`run_rollup.sh`, `run_verifier_service.sh`, etc.), and supports both global and per-service actions:

```bash
cargo run -p sov-rollup-ligero --bin rollup-ligero-service-controller --release
```

```bash
curl -X POST http://127.0.0.1:9090/start
curl -X POST http://127.0.0.1:9090/stop
curl -X POST http://127.0.0.1:9090/restart
curl -X POST http://127.0.0.1:9090/clean
curl -X POST http://127.0.0.1:9090/clean-database
curl -X POST http://127.0.0.1:9090/reset-tee

# Per-service controls
curl -X POST http://127.0.0.1:9090/start/rollup
curl -X POST http://127.0.0.1:9090/stop/worker
curl -X POST http://127.0.0.1:9090/restart/indexer
curl -X POST http://127.0.0.1:9090/start/mcp
curl -X POST http://127.0.0.1:9090/start/proof-pool

# Discover known services and controller process status
curl http://127.0.0.1:9090/services
```

Notes:
- Managed service IDs: `oracle`, `rollup`, `worker`, `fvk`, `indexer`, `proof-pool`, `mcp`, `metrics`
- Bind address: `SERVICE_CONTROLLER_BIND` (default `127.0.0.1:9090`)
- Auto-start default services on controller start: set `SERVICE_CONTROLLER_AUTO_START=1`
- Per-service remote mode:
  - Set `SERVICE_WORKER_REMOTE=1` to disable local start/stop/restart for worker.
  - Set `SERVICE_WORKER_URL=https://<remote-host>:8080` to show/check the remote endpoint in `/health`.
  - Legacy alias `SERVICE_VERIFIER_REMOTE` / `SERVICE_VERIFIER_URL` is also supported.
- TEE reset action:
  - Set `TEE_RESET_URL` (default: `http://74.235.106.62:9898/reset`).
  - Set `TEE_RESET_BEARER_TOKEN` (required for `/reset-tee`; alias: `TEE_RESET_TOKEN`).
- Database cleanup action:
  - Set `DA_CONNECTION_STRING` on the controller process (PostgreSQL URL).
  - `/clean-database` drops all tables with `CASCADE` from databases: `da`, `indexer`, `fvk`, `mcp_sessions`.
- `Clean Data`, `/clean-database`, and `/reset-tee` only run when all managed services are stopped.
- `/clean` removes `demo_data` and only runs when services are stopped

### Linux Services

Systemd unit templates live in `examples/rollup-ligero/services`, including:
- `rollup-ligero-service-controller.service`

## Configuration

The rollup configuration is in `rollup_config.toml`. Key settings:

- `max_batch_size_bytes = 5242880` (5MB) - Increased to support large Ligero proofs
- HTTP body limit: 10MB (configured in code)

## Environment Variables

- `SOV_PROVER_MODE`: Controls proving behavior
  - `skip`: Skip proving entirely
  - `execute`: Execute transactions without generating proofs
  - `prove`: Full proof generation and verification
  
- `SKIP_GUEST_BUILD`: Skip building guest programs
  - `1`, `true`, or `ligero`: Skip Ligero guest builds

- `LIGERO_SDK_PATH`: Path to Ligero SDK (default: `<ligero-prover>/sdk`)

## Differences from demo-rollup

This crate differs from the standard `demo-rollup` in the following ways:

1. **No RISC0/SP1**: Only uses Ligero zkVM
2. **No prover sub-crates**: No `provers/risc0` or `provers/sp1` directories
3. **Simplified build**: Only builds Ligero guest programs (via `sov-ligero-adapter`)
4. **Larger proof size limits**: Configured to handle 3-4MB Ligero proofs

## Using sov-cli

You can use the standard `sov-cli` from the main demo-rollup to interact with this rollup:

```bash
# Build the CLI from demo-rollup
cd ../demo-rollup
cargo build --release --bin sov-cli --features arbitrary

# Create a wallet
../../target/release/sov-cli keys import

# Import a transaction
../../target/release/sov-cli transactions import value_tx.json

# Publish transactions
../../target/release/sov-cli transactions publish-batch http://127.0.0.1:12345
```

The CLI is compatible because both rollups use the same STF (State Transition Function) and modules.

## Testing

```bash
# Run tests
cargo test

# Run with specific features
cargo test --features arbitrary
```

## Cleaning Up

```bash
# Remove rollup data
rm -rf demo_data mock_da.sqlite

# Remove CLI wallet
rm -rf ~/.sov-cli-wallet
```

## Modules

This rollup includes all standard Sovereign SDK modules:

- **Bank**: Token transfers and balances
- **Accounts**: Account management
- **Sequencer**: Transaction sequencing
- **EVM**: Ethereum Virtual Machine support
- **Value Setter ZK**: Example module using Ligero proofs on-chain
- And more...

## Architecture

```
rollup-ligero/
├── src/
│   ├── main.rs          # Entry point
│   ├── lib.rs           # Library exports
│   ├── mock_rollup.rs   # Mock DA rollup implementation
│   └── zk.rs            # Ligero host args
├── Cargo.toml           # Dependencies (Ligero only)
├── build.rs             # Build configuration
└── rollup_config.toml  # Rollup settings
```

## Troubleshooting

### "Emscripten not found"
Make sure you've activated the Emscripten environment:
```bash
source /path/to/emsdk/emsdk_env.sh
```

### "Ligero SDK library not found"
Build the Ligero SDK or set `SKIP_GUEST_BUILD=ligero`:
```bash
export SKIP_GUEST_BUILD=ligero
```

### "Payload Too Large" errors
Increase `max_batch_size_bytes` in `rollup_config.toml`.

### Compilation errors
Make sure you're using the correct Rust toolchain:
```bash
rustup show  # Should match rust-toolchain.toml
```

## License

Sovereign Permissionless Commercial License

## Links

- [Sovereign SDK](https://github.com/Sovereign-Labs/sovereign-sdk)
- [Ligero Prover](https://github.com/ligeroinc/ligero-prover)
- [Emscripten](https://emscripten.org/)
