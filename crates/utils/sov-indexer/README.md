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
   - `MIDNIGHT_FVK_SERVICE_URL` (optional): midnight-fvk-service base URL, default `http://127.0.0.1:8088`
   - `MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN` (optional): if set, indexer fetches missing per-wallet FVKs on-demand from midnight-fvk-service (and caches them in `fvk_registry`)
   - `INDEX_DB_RESET` (optional): set to `1`/`true` to drop all index tables before startup (works for sqlite/postgresql). The `fvk_registry` table is preserved.
   - `SOV_INDEXER_SYNC_INTERVAL_MS` (optional): interval for indexing newly accepted worker rows, default `1000`.
   - `SOV_INDEXER_RECONCILE_INTERVAL_SECS` (optional): interval for best-effort accepted-row reconciliation, default `60`.
   - `SOV_INDEXER_STARTUP_BACKFILLS_ENABLED` (optional): set to `true` to run historical encrypted-note/nullifier maintenance backfills on startup, default `false`.

2) Run the service:
   ```bash
   cargo run -p sov-indexer
   ```

3) Endpoints:
   - Health: `GET /health`
     - Returns `{ "status": "ok" }`
   - Wallet activity: `POST /wallets/:address?limit=&cursor=&type=`
     - `address`: bech32 L2 address
     - Query params:
       - `limit`: optional, default 50, max 200
       - `cursor`: opaque base64 from previous response for pagination
       - `type`: optional filter: `deposit` or `withdraw`
     - JSON body:
       - `vfk`: 32-byte hex full viewing key (optional)
     - If `vfk` is provided, `decrypted_notes` are returned (unshielded mode); otherwise only `encrypted_notes` are returned (shielded mode)
     - Response includes `total` (count of matching records before pagination)
   - Wallet balance: `POST /wallets/:address/balance`
     - `address`: bech32m privacy pool address (`privpool1...`)
  - JSON body:
   - `nf_key`: 32-byte hex nullifier key (required)
   - `vfk`: 32-byte hex full viewing key (optional)
  - Returns `{ "balance": "...", "unspent_notes": [...] }`
  - Note: the indexer cannot verify that `nf_key` matches the address. If decrypted notes are already stored in the index, `vfk` can be omitted; otherwise some transfer outputs may be missing.

## FVK Decryption (Optional)

The indexer can decrypt encrypted notes using Full Viewing Keys (FVKs). Each shielded address
has its own FVK, so the indexer supports multiple FVKs via a registry.

### Configuration Options

1. **Auto-fetch from `midnight-fvk-service`** (recommended):
   Set:
   - `MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN` (must match the service)
   - `MIDNIGHT_FVK_SERVICE_URL` (optional; defaults to `http://127.0.0.1:8088`)

   When enabled, the indexer looks at each encrypted note’s `fvk_commitment` and fetches the corresponding private `fvk` from the service, then stores it in the `fvk_registry` table for reuse.

2. **FVK Config File** (offline/manual):
   Set `FVK_CONFIG_FILE` to point to a JSON file:
   ```bash
   FVK_CONFIG_FILE=./vfk_config.json cargo run -p sov-indexer
   ```

### FVK Config File Format

Create a JSON file with your FVKs:

```json
{
  "fvks": [
    {
      "fvk": "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1",
      "shielded_address": "privpool1qypqxpq9qcrsszg2pvxq6rs..."
    },
    {
      "fvk": "a1b2c3d4e5f6789012345678901234567890123456789012345678901234abcd"
    }
  ]
}
```

- `fvk`: Required - the 32-byte FVK as 64 hex characters
- `shielded_address`: Optional - the associated shielded address

### How It Works

Each encrypted note contains an `fvk_commitment` field (a hash of the FVK used to encrypt it).
When indexing, the indexer:
1. Reads the `fvk_commitment` from each encrypted note
2. Looks up the matching FVK in the registry
3. Decrypts the note if a matching FVK is found

This allows the indexer to decrypt notes for multiple addresses, each with their own FVK.

### FVK Registry Table

FVKs are stored in the `fvk_registry` table with columns:
- `fvk_commitment` (primary key): Hash of the FVK for fast lookup
- `fvk`: The actual 32-byte FVK (hex-encoded)
- `shielded_address`: The associated shielded address (optional)

### REST API for FVK Management

You can add/remove FVKs at runtime without restarting the indexer:

**List FVKs (paginated):**
```bash
curl "http://localhost:13100/fvks?limit=100"
```

**Fetch next page:**
```bash
# Replace <next_cursor> with the previous response's `next_cursor`
curl "http://localhost:13100/fvks?limit=100&cursor=<next_cursor>"
```

The response includes:
- `count` (items in this page)
- `total_count` (registry size)
- `next_cursor` (null on last page)

**Add a new FVK:**
```bash
curl -X POST http://localhost:13100/fvks \
  -H "Content-Type: application/json" \
  -d '{
    "fvk": "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1",
    "shielded_address": "privpool1..."
  }'
```

**Add a new FVK with commitment verification:**
```bash
# If you know the expected fvk_commitment, you can provide it to verify
# the FVK is correct before adding it to the registry
curl -X POST http://localhost:13100/fvks \
  -H "Content-Type: application/json" \
  -d '{
    "fvk": "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1",
    "fvk_commitment": "abc123...",
    "shielded_address": "privpool1..."
  }'
```

The API validates:
- FVK format: must be valid hex, exactly 32 bytes (64 hex chars)
- Commitment match: if `fvk_commitment` is provided, it must match the commitment computed from the FVK
- No duplicates: returns 409 Conflict if the FVK is already registered

**Delete a FVK:**
```bash
curl -X DELETE http://localhost:13100/fvks/<fvk_commitment>
```

Changes take effect immediately - new transactions will be decrypted using the updated registry,
and existing indexed deposits/transfers with missing recipients are backfilled from encrypted notes.
The registry uses DashMap for lock-free concurrent access during indexing.

Notes
- On startup, if the DA DB is not ready, the service logs a warning and retries in the background.
- The index DB schema is created automatically on first run.
- VFK decryption only works in `sync` mode (local index database).
- VFKs from config files are persisted to the database for reuse across restarts.
