# MCP Server

Model Context Protocol (MCP) server for the Sovereign SDK L2 rollup.

## Overview

This server exposes L2 wallet operations through the MCP protocol, enabling AI assistants and other clients to interact with the rollup network. Features include wallet management, transaction submission, and privacy-preserving transfers with ZK proof generation.

### Session Isolation (Multi-Provider)

`mcp-external` uses standard MCP HTTP sessions (`Mcp-Session-Id`). Each MCP session has its own isolated wallet/key state, so a single running `mcp-external` instance can safely serve multiple independent providers in parallel.

Sessions start with no wallet loaded. In a fresh session, call `createWallet` (optionally auto-funded) or `restoreWallet` to set per-session keys.

Reconnecting with the same `Mcp-Session-Id` continues using the same per-session wallet state while the server is running and the session remains open.

#### User-defined `Mcp-Session-Id`

Clients may either:
- omit `Mcp-Session-Id` and let the server generate one (returned in the `Mcp-Session-Id` response header on `initialize`), or
- provide their own `Mcp-Session-Id` on `initialize`.

If the provided `Mcp-Session-Id` already exists, the server resumes that session. If it doesn't exist, the server creates a fresh session and binds it to that id for future requests (within the current server process).

Note: the first request for a new session must be the MCP `initialize` request.

## Running the Server

### Prerequisites

- Rust toolchain
- Running Sovereign SDK L2 rollup node
- Ligero proof service (`ligero-http-server`) running (default: `http://127.0.0.1:1313`)

### Configuration

Configure the following environment variables:

- `MCP_SERVER_BIND_ADDRESS` - Server bind address (default: `127.0.0.1:3000`)
- `START_WITH_NEW_WALLET` - Deprecated/ignored (sessions start empty)
- `WALLET_PRIVATE_KEY` - Deprecated/ignored (use `restoreWallet` per session)
- `ADMIN_WALLET_PRIVATE_KEY` - Optional admin wallet private key used only to auto-fund newly created wallets
- `ROLLUP_RPC_URL` - L2 rollup RPC endpoint
- `VERIFIER_URL` - Transaction verifier service endpoint
- `INDEXER_URL` - Transaction indexer endpoint
- `LIGERO_PROOF_SERVICE_URL` - Ligero proof service base URL (default: `http://127.0.0.1:1313`)
- `LIGERO_PROGRAM_PATH` - Ligero circuit name or program specifier (default: `note_spend_guest`)
- `PRIVPOOL_SPEND_KEY` - Deprecated/ignored (use `restoreWallet` per session)
- `POOL_FVK_PK` - Optional 32-byte `ed25519` public key enabling pool-signed viewer commitments (must match `midnight-fvk-service` signer)
- `MIDNIGHT_FVK_SERVICE_URL` - Optional `midnight-fvk-service` base URL (default `http://127.0.0.1:8088`)
- `AUTO_FUND_DEPOSIT_AMOUNT` - Optional amount (in dust) to auto-fund a new wallet when `createWallet` runs (best-effort).
- `AUTO_FUND_GAS_RESERVE` - Optional gas reserve (in dust) added to the L2 funding transfer for auto-funding (default: 1000000000000). Values below the default are clamped to ensure the deposit can reserve gas.
- `MCP_TRANSFER_WAIT_MODE` - Optional post-submit wait mode for `send`: `sequencer` (default) or `none`.
- `MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN` - Admin token (shared with FVK service). If set, POST endpoints on `/authority` require `Authorization: Bearer <token>`.
- `METRICS_API_URL` - Optional `sov-metrics-api` base URL used by `GET /authority/tps` (e.g. `http://127.0.0.1:13200`).

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
- `getTransactions` - List all transactions from the indexer
- `walletStatus` - Sync status and balances
- `createWallet` / `restoreWallet` - Manage wallet keys
- `removeWallet` - Clear loaded wallet (enables create/restore again)

If `AUTO_FUND_DEPOSIT_AMOUNT` is set (or the legacy `STARTUP_DEPOSIT_AMOUNT`) and `ADMIN_WALLET_PRIVATE_KEY` is provided, calling `createWallet` triggers a best-effort auto-fund sequence: the admin wallet sends L2 tokens to the new wallet (deposit amount + gas reserve), then the new wallet deposits the configured amount into the privacy pool. When `START_WITH_NEW_WALLET=true`, the same auto-fund flow runs during startup.

### Authority (HTTP)

For compatibility with the `midnight-sim/mockmcp` API shape, `mcp-external` also exposes a small set of non-MCP HTTP endpoints:

- `GET /authority` - Discover available authority endpoints
- `GET /authority/info` - List frozen (blacklisted) privacy addresses (returns `["addr1", "addr2"]`)
- `GET /authority/accounts` - List all accounts with wallet data (MockMCP-compatible format)
- `POST /authority/freeze` - Freeze a privacy address (requires `MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN` + `ADMIN_WALLET_PRIVATE_KEY`)
- `POST /authority/thaw` - Unfreeze a privacy address (requires `MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN` + `ADMIN_WALLET_PRIVATE_KEY`)
- `GET /authority/tps` - TPS over 1m/5m/15m windows (requires `METRICS_API_URL`)

#### GET /authority/accounts Response Format

Returns an array of `[wallet_id, wallet_data]` tuples matching the MockMCP spec:

```json
[
  [
    "fvk_commitment_hex",
    {
      "authorityVFK": "full_viewing_key_hex",
      "balance": "10000000",
      "frozen": null,
      "lastSend": "2026-01-27T12:00:00Z",
      "pendingBalance": "0",
      "privacyAddress": "privpool1...",
      "privacySpendKey": null
    }
  ]
]
```

**Notes:**
- Accounts are sourced from the indexer's FVK registry (registered viewing keys)
- `privacySpendKey` is always `null` for security (private keys are never exposed)
- `frozen` is `null` if not frozen, or a string with the freeze reason
- `lastSend` defaults to `"0000-01-01T00:00:00Z"` if no transfer history exists

#### GET /authority/info Response Format

Returns a simple array of frozen privacy addresses:

```json
["privpool1...", "privpool1..."]
```

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
