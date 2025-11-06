# sov-indexer

A small service that indexes wallet activity from the Sovereign rollup’s Midnight DA database and serves a simple HTTP API for querying transactions by address.

What it does
- Connects to the Midnight DA database (worker_verified_transactions).
- Builds a local, normalized index (SQLite by default) with:
  - Events (deposit/withdraw)
  - Involvement (who is sender/recipient, with direction in/out)
  - Typed details for Midnight deposit/withdraw
- Continuously syncs new items in the background.
- Exposes a minimal REST API to fetch transactions for a wallet address.

Status coverage
- Indexes Midnight deposits (sender/out) and withdrawals (recipient/in and sender/out).
- Transfers are not indexed yet (pending DA support).

Quick start
1) Set configuration via environment variables (can also be in a `.env` file):
   - `DA_CONNECTION_STRING` (required): e.g. `sqlite://examples/rollup-ligero/demo_data/da.sqlite?mode=rwc`
   - `INDEX_DB` (optional): local index DB, default `sqlite://wallet_index.sqlite?mode=rwc`
   - `INDEXER_BIND` (optional): listen address, default `0.0.0.0:13100`

2) Run the service:
   ```bash
   cargo run -p sov-indexer
   ```

3) Endpoints:
   - Health: `GET /health`
     - Returns `{ "status": "ok" }`
   - Wallet activity: `GET /wallets/:address/txs?limit=&cursor=&type=`
     - `address`: bech32 L2 address
     - `limit`: optional, default 50, max 200
     - `cursor`: opaque base64 from previous response for pagination
     - `type`: optional filter: `deposit` or `withdraw`

Notes
- On startup, if the DA DB is not ready, the service logs a warning and retries in the background.
- The index DB schema is created automatically on first run.

