# Midnight Proof Pool Service

Pre-generates a pool of **verified** Midnight privacy self-transfer transactions and keeps them in
the proof verifier DB as `pending` (verifier runs in **defer** mode). A `/send` call flushes a
requested number of pending transactions to the sequencer, and the service refills back to
`MAX_PROOFS`.

## Endpoints

- `GET /status?token=...`
  - Returns `{ max_proofs, ready_proofs }`
- `POST /send?token=...` with JSON body `{ "proof_quantity": N }`
  - Flushes up to `N` pending txs to the sequencer (via verifier `/midnight-privacy/flush?limit=N`)
  - Also supports: `GET /send?token=...&proof_quantity=N` (phone-friendly)

## Required env

- `AUTH_TOKEN` - required; used as `?token=...` on all endpoints
- `MAX_PROOFS` - number of wallets / pending proofs to maintain
- `ADMIN_WALLET_PRIVATE_KEY` - hex private key used to fund generated wallets
- `ROLLUP_RPC_URL` - rollup node base URL (default: `http://127.0.0.1:12346`)
- `DA_CONNECTION_STRING` - **must match the node's DA DB** (e.g. `sqlite://.../demo_data/da.sqlite?mode=rwc`)
- `LIGERO_PROOF_SERVICE_URL` - ligero-http-server base URL (default: `http://127.0.0.1:1313`)

## Optional env

- `PROOF_POOL_BIND_ADDR` - service bind address (default: `127.0.0.1:8888`)
- `INDEXER_URL` - indexer base URL (default: `http://localhost:13100`)
- `DEPOSIT_AMOUNT` - initial shielded deposit per wallet (default: `200`)
- `AUTO_FUND_GAS_RESERVE` - extra L2 funding per wallet, added to `DEPOSIT_AMOUNT` (default: `10000000`, min `DEFAULT_MAX_FEE`)
- `TOPUP_GAS_RESERVE` - L2 top-up amount when a wallet balance drops below `DEFAULT_MAX_FEE` (default: `AUTO_FUND_GAS_RESERVE`)
- `SETUP_CONCURRENCY` - concurrent wallet funding/deposit setup (default: `10`)
- `MAX_CONCURRENT_PROOFS` - concurrent proof generations (default: `5`)
- `LIGERO_PROGRAM_PATH` - circuit name or wasm path (default: `note_spend_guest`)
- `VERIFIER_PROVER_SERVICE_URL` - overrides verifier-side remote `/verify` URL (default: `LIGERO_PROOF_SERVICE_URL`)

## Run

```bash
AUTH_TOKEN=secret \
MAX_PROOFS=100 \
ADMIN_WALLET_PRIVATE_KEY=0x... \
ROLLUP_RPC_URL=http://127.0.0.1:12346 \
DA_CONNECTION_STRING="sqlite://$(pwd)/examples/rollup-ligero/demo_data/da.sqlite?mode=rwc" \
LIGERO_PROOF_SERVICE_URL=http://127.0.0.1:1313 \
cargo run -p midnight-proof-pool-service --release
```

```bash
curl "http://127.0.0.1:8888/status?token=secret"
curl "http://127.0.0.1:8888/send?token=secret&proof_quantity=25"
```
