# MCP Server

Model Context Protocol (MCP) server for the Sovereign SDK L2 rollup.

## Overview

This server exposes L2 wallet operations through the MCP protocol, enabling AI assistants and other clients to interact with the rollup network. Features include wallet management, transaction submission, and privacy-preserving transfers with ZK proof generation.

## Running the Server

### Prerequisites

- Rust toolchain
- Running Sovereign SDK L2 rollup node
- Ligero binaries are automatically provided by the `ligero-runner` crate (no manual setup needed)

### Configuration

Configure the following environment variables:

- `MCP_SERVER_BIND_ADDRESS` - Server bind address (default: `127.0.0.1:3000`)
- `WALLET_PRIVATE_KEY` - Hex-encoded private key for wallet operations
- `ROLLUP_RPC_URL` - L2 rollup RPC endpoint
- `VERIFIER_URL` - Transaction verifier service endpoint
- `INDEXER_URL` - Transaction indexer endpoint
- `PRIVPOOL_SPEND_KEY` - Privacy pool spend key (hex or bech32m address)
- `AUTHORITY_FVK` - Optional authority viewing key for note decryption
- `AUTO_FUND_DEPOSIT_AMOUNT` - Optional amount (in dust) to auto-fund a new wallet when `createWallet` runs (best-effort).

### Start the Server

```bash
cargo run -p mcp-external
```

The MCP endpoint will be available at `http://<bind_address>/mcp`.

## API Endpoints

The server exposes the following MCP tools:

- `walletAddress` - Get the privacy pool address
- `walletBalance` - Get the privacy balance and unspent notes
- `getWalletConfig` - Retrieve wallet configuration (node/indexer/proof server/log paths)
- `send` - Non-blocking privacy transfer using the first unspent note
- `getTransaction` - Retrieve transaction details by hash or derived UUID
- `getTransactionStatus` - Query a transaction by local database UUID
- `getTransactions` - List all transactions from the local store
- `walletStatus` - Sync status and balances
- `verifyTransaction` - Verify receipt and decrypt amount when possible
- `createWallet` / `restoreWallet` - Manage wallet keys

If `AUTO_FUND_DEPOSIT_AMOUNT` is set (or the legacy `STARTUP_DEPOSIT_AMOUNT`), calling `createWallet` will kick off a best-effort deposit to fund the new privacy address.

### Local transaction store

An in-memory SQLite database (`sqlite::memory:?cache=shared`) tracks all wallet transactions and is kept in sync with the indexer every ~30 seconds. `send` inserts an `initiated` row immediately and continues in the background; indexer-derived records are upserted by `tx_identifier` to avoid uniqueness conflicts. The `id` returned by `send`/`getTransactions` is a local UUID; the blockchain hash is exposed as `txIdentifier` once known.

## Docker

Build the Docker image from the repo root (uses `crates/mcp-external/Dockerfile`):

```bash
docker build -f crates/mcp-external/Dockerfile -t dcspark/integration-e8d6e6728 -t dcspark/integration-e8d6e6728:latest .
```

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
