# Midnight Proof Pool Service

Pre-generates a pool of **verified** Midnight privacy self-transfer transactions and keeps them in
the proof verifier DB as `pending` (verifier runs in **defer** mode). A `/send` call flushes a
requested number of pending transactions to the sequencer, and the service refills back to
`MAX_PROOFS`.

## Endpoints

- `GET /status?auth_token=...`
  - Returns `{ max_proofs, ready_proofs, proof_generation_active, proof_generation_state, proof_generation_interval_ms, proof_generation_batch_size }`
- `POST /max_proofs?auth_token=...` with JSON body `{ "max_proofs": N }`
  - Updates the target `MAX_PROOFS` (the service scales wallets up in bounded batches and refills pending proofs to match)
- Also supports: `GET /max_proofs?auth_token=...&max_proofs=N` (phone-friendly)
- `POST /proof_generation?auth_token=...` with optional JSON body fields:
  - `{ "state": "started" | "stopped", "interval_ms": N, "batch_size": M }`
  - `state` toggles background proof refill generation on/off (send/burst keep working either way)
  - `interval_ms` sets minimum delay between generation batches (`0` means no interval throttle)
  - `batch_size` sets max number of proofs started per refill batch/tick (must be `> 0`)
- Also supports query params: `GET /proof_generation?auth_token=...&state=started|stopped&interval_ms=N&batch_size=M` (phone-friendly)
- `POST /send?auth_token=...` with JSON body `{ "proof_quantity": N }`
  - Flushes up to `N` pending txs to the sequencer (via verifier `/midnight-privacy/flush?limit=N`)
- Also supports: `GET /send?auth_token=...&proof_quantity=N` (phone-friendly)
- `POST /burst?auth_token=...` with JSON body `{ "proof_quantities": [2, 5, 10] }`
  - Runs a timed burst: flushes `2`, then `5`, then `10` pending txs, waiting **2 seconds** between steps
- Also supports: `GET /burst?auth_token=...&proof_quantities=2,5,10` (phone-friendly)

## Required env

- `AUTH_TOKEN` - required; used as `?auth_token=...` on all endpoints
- `MAX_PROOFS` - initial number of wallets / target pending proofs to maintain (can be updated at runtime via `/max_proofs`)
- `ADMIN_WALLET_PRIVATE_KEY` - hex private key used to fund generated wallets
- `ROLLUP_RPC_URL` - rollup node base URL (default: `http://127.0.0.1:12346`)
- `DA_CONNECTION_STRING` - **must match the node's DA DB** (e.g. `sqlite://.../demo_data/da.sqlite?mode=rwc`)
- `LIGERO_PROOF_SERVICE_URL` - ligero-http-server base URL (default: `http://127.0.0.1:8080`)

## Optional env

- `PROOF_POOL_BIND_ADDR` - service bind address (default: `127.0.0.1:11235`)
- `INDEXER_URL` - indexer base URL (default: `http://localhost:13100`)
- `DEPOSIT_AMOUNT` - initial shielded deposit per wallet (default: `200`)
- `AUTO_FUND_GAS_RESERVE` - extra L2 funding per wallet, added to `DEPOSIT_AMOUNT` (default: `10000000`, min `DEFAULT_MAX_FEE`)
- `TOPUP_GAS_RESERVE` - L2 top-up amount when a wallet balance drops below `DEFAULT_MAX_FEE` (default: `AUTO_FUND_GAS_RESERVE`)
- `SETUP_CONCURRENCY` - concurrent wallet funding/deposit setup (default: `10`)
- `WALLET_SETUP_BATCH_SIZE` - max number of wallets created/funded/deposited per scale-up batch (default: `5`)
- `WALLET_SETUP_BACKOFF_MS` - delay between scale-up checks/batches and retry backoff when sequencer is not ready (default: `1000`)
- `SEQUENCER_READY_CHECK_TIMEOUT_MS` - timeout for `/sequencer/ready` backpressure check before each scale-up batch (default: `2000`)
- `PROOF_GENERATION_INTERVAL_MS` - minimum delay between refill batches in the refill loop (default: `0`, no interval throttle)
- `PROOF_GENERATION_BATCH_SIZE` - max proofs started per refill batch/tick (default: `1`)
- `MAX_CONCURRENT_PROOFS` - concurrent proof generations (default: `5`)
- `PROOF_POOL_TREE_RESOLVE_RETRY_ATTEMPTS` - extra retries for transient tree lag / stale anchor root during self-transfer generation (default: `1`)
- `PROOF_POOL_TREE_RESOLVE_RETRY_DELAY_MS` - delay between those retries in ms (default: `750`)
- `LIGERO_PROGRAM_PATH` - circuit name or wasm path (default: `note_spend_guest`)
- `VERIFIER_PROVER_SERVICE_URL` - overrides verifier-side remote `/verify` URL (default: `LIGERO_PROOF_SERVICE_URL`)
- `POOL_FVK_PK` - when set, enables pool-signed viewer commitments; the service will fetch 1 viewer FVK per wallet from `MIDNIGHT_FVK_SERVICE_URL`
- `MIDNIGHT_FVK_SERVICE_URL` - midnight-fvk-service base URL (default: `http://127.0.0.1:8088`)

## Run

```bash
AUTH_TOKEN=secret \
MAX_PROOFS=100 \
ADMIN_WALLET_PRIVATE_KEY=0x... \
ROLLUP_RPC_URL=http://127.0.0.1:12346 \
DA_CONNECTION_STRING="sqlite://$(pwd)/examples/rollup-ligero/demo_data/da.sqlite?mode=rwc" \
LIGERO_PROOF_SERVICE_URL=http://127.0.0.1:8080 \
cargo run -p midnight-proof-pool-service --release
```

```bash
curl "http://127.0.0.1:11235/status?auth_token=secret"
curl "http://127.0.0.1:11235/max_proofs?auth_token=secret&max_proofs=25"
curl "http://127.0.0.1:11235/proof_generation?auth_token=secret&state=stopped"
curl "http://127.0.0.1:11235/proof_generation?auth_token=secret&state=started"
curl "http://127.0.0.1:11235/proof_generation?auth_token=secret&interval_ms=500&batch_size=3"
curl "http://127.0.0.1:11235/send?auth_token=secret&proof_quantity=25"
curl "http://127.0.0.1:11235/burst?auth_token=secret&proof_quantities=2,5,10"
```
