# MCP Server

Model Context Protocol (MCP) server for the Sovereign SDK L2 rollup.

## Overview

This server exposes L2 wallet operations through the MCP protocol, enabling AI assistants and other clients to interact with the rollup network. Features include wallet management, transaction submission, and privacy-preserving transfers with ZK proof generation.

## Running the Server

### Prerequisites

- Rust toolchain
- Running Sovereign SDK L2 rollup node
- Ligero prover binaries and shader files

### Configuration

Configure the following environment variables:

- `MCP_SERVER_BIND_ADDRESS` - Server bind address (default: `127.0.0.1:3000`)
- `WALLET_PRIVATE_KEY` - Hex-encoded private key for wallet operations
- `ROLLUP_RPC_URL` - L2 rollup RPC endpoint
- `VERIFIER_URL` - Transaction verifier service endpoint
- `INDEXER_URL` - Transaction indexer endpoint
- `PRIVPOOL_SPEND_KEY` - Privacy pool spend key (hex or bech32m address)
- `AUTHORITY_VFK` - Optional authority viewing key for note decryption

#### Ligero Configuration (Optional)

Ligero binaries and programs are **automatically discovered** from the `ligero-runner` crate. 
You can optionally override them:

- `LIGERO_PROGRAM_PATH` - Circuit name (e.g. `note_spend_guest`) or full path to `.wasm` file
- `LIGERO_PROVER_BIN` - Override path to `webgpu_prover` binary
- `LIGERO_SHADER_PATH` - Override path to shader directory

### Start the Server

```bash
cargo run -p mcp
```

The MCP endpoint will be available at `http://<bind_address>/mcp`.

## API Endpoints

The server exposes the following MCP tools:

- `walletAddress` - Get wallet address and privacy pool address
- `walletBalance` - Get complete balance state (transparent L2 + privacy pool balances, unspent notes, transaction counts)
- `getWalletConfig` - Retrieve wallet configuration (includes default token ID)
- `sendFunds` - Send tokens to another address on the L2 (standard Bank transfer)
- `deposit` - Deposit funds into the privacy pool
- `transfer` - Transfer funds within the privacy pool (requires ZK proof)
- `getTransaction` - Retrieve transaction details by hash
- `getTransactions` - List all transactions for a wallet
- `getTransactionWithSelectivePrivacy` - Decrypt privacy transaction notes (requires authority VFK)

## Testing

Run the fast, self-contained tests:

```bash
cargo test -p mcp
```

Integration tests (rollup/indexer/verifier + Ligero) are ignored by default. Run them explicitly with:

```bash
cargo test --all-targets -- --ignored
```

Note: `-- --ignored` runs only the ignored tests; non-ignored tests will be reported as "filtered out". To run everything, execute both commands above. The integration suite expects these env vars/files to exist:

- `ROLLUP_RPC_URL`, `VERIFIER_URL`, `INDEXER_URL`
- `WALLET_PRIVATE_KEY`, `PRIVPOOL_SPEND_KEY`

Ligero binaries are auto-discovered from `ligero-runner`. Optional overrides:

- `LIGERO_PROGRAM_PATH` - Circuit name (e.g. `note_spend_guest`) or path to `.wasm`
- `LIGERO_PROVER_BIN` - Override `webgpu_prover` binary path
- `LIGERO_SHADER_PATH` - Override shader directory path

Set `RUST_LOG=debug` for verbose logging during development.
