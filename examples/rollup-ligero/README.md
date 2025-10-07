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

- `LIGERO_SDK_PATH`: Path to Ligero SDK (default: `../../../../../ligero-vm/ligero-prover/sdk`)

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

