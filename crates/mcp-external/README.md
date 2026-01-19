# MCP Server

Model Context Protocol (MCP) server for the Sovereign SDK L2 rollup.

## Overview

This server exposes L2 wallet operations through the MCP protocol, enabling AI assistants and other clients to interact with the rollup network. Features include wallet management, transaction submission, and privacy-preserving transfers with ZK proof generation.

## Running the Server

### Prerequisites

- Rust toolchain
- Running Sovereign SDK L2 rollup node
- Ligero proof service (`ligero-http-server`) running (default: `http://127.0.0.1:1313`)

### Configuration

Configure the following environment variables:

- `MCP_SERVER_BIND_ADDRESS` - Server bind address (default: `127.0.0.1:3000`)
- `WALLET_PRIVATE_KEY` - Hex-encoded private key for wallet operations
- `ADMIN_WALLET_PRIVATE_KEY` - Optional admin wallet private key used only to auto-fund newly created wallets
- `ROLLUP_RPC_URL` - L2 rollup RPC endpoint
- `VERIFIER_URL` - Transaction verifier service endpoint
- `INDEXER_URL` - Transaction indexer endpoint
- `LIGERO_PROOF_SERVICE_URL` - Ligero proof service base URL (default: `http://127.0.0.1:1313`)
- `LIGERO_PROGRAM_PATH` - Ligero circuit name or program specifier (default: `note_spend_guest`)
- `PRIVPOOL_SPEND_KEY` - Privacy pool spend key (hex or bech32m address)
- `POOL_FVK_PK` - Optional 32-byte `ed25519` public key enabling pool-signed viewer commitments (must match `midnight-fvk-service` signer)
- `MIDNIGHT_FVK_SERVICE_URL` - Optional `midnight-fvk-service` base URL (default `http://127.0.0.1:8088`)
- `AUTO_FUND_DEPOSIT_AMOUNT` - Optional amount (in dust) to auto-fund a new wallet when `createWallet` runs (best-effort).
- `AUTO_FUND_GAS_RESERVE` - Optional gas reserve (in dust) added to the L2 funding transfer for auto-funding (default: 1000000000000). Values below the default are clamped to ensure the deposit can reserve gas.

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
- `send` - Privacy transfer using the first unspent note
- `getTransaction` - Retrieve transaction details by hash or derived UUID (legacy)
- `getTransactions` - List all transactions from the indexer
- `walletStatus` - Sync status and balances
- `createWallet` / `restoreWallet` - Manage wallet keys

If `AUTO_FUND_DEPOSIT_AMOUNT` is set (or the legacy `STARTUP_DEPOSIT_AMOUNT`) and `ADMIN_WALLET_PRIVATE_KEY` is provided, calling `createWallet` triggers a best-effort auto-fund sequence: the admin wallet sends L2 tokens to the new wallet (deposit amount + gas reserve), then the new wallet deposits the configured amount into the privacy pool.

### Transactions

Transactions are fetched directly from the indexer when requested. `send` returns the rollup transaction hash, and the `id` field in transaction records matches that hash.

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

Integration tests (rollup/indexer/verifier + Ligero proof service) are ignored by default. Run them explicitly with:

```bash
cargo test --all-targets -- --ignored
```

Note: `-- --ignored` runs only the ignored tests; non-ignored tests will be reported as "filtered out". To run everything, execute both commands above. The integration suite expects these env vars/files to exist:

- `ROLLUP_RPC_URL`, `VERIFIER_URL`, `INDEXER_URL`
- `WALLET_PRIVATE_KEY`, `PRIVPOOL_SPEND_KEY`
- `LIGERO_PROOF_SERVICE_URL` (defaults to `http://127.0.0.1:1313`)
- `LIGERO_PROGRAM_PATH` (defaults to `note_spend_guest`)

Set `RUST_LOG=debug` for verbose logging during development.
