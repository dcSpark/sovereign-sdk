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
   - `AUTHORITY_VFK` (optional): 32-byte hex authority viewing key for decrypting encrypted notes

2) Run the service:
   ```bash
   cargo run -p sov-indexer
   ```

3) Endpoints:
   - Health: `GET /health`
     - Returns `{ "status": "ok" }`
   - Wallet activity: `GET /wallets/:address?limit=&cursor=&type=`
     - `address`: bech32 L2 address
     - `limit`: optional, default 50, max 200
     - `cursor`: opaque base64 from previous response for pagination
     - `type`: optional filter: `deposit` or `withdraw`
   - Wallet balance: `POST /wallets/:address/balance`
     - `address`: bech32m privacy pool address (`privpool1...`)
     - JSON body:
       - `spend_sk`: 32-byte hex spending secret key (required)
       - `vfk`: 32-byte hex full viewing key (optional)
     - Returns `{ "balance": "...", "unspent_notes": [...] }`

## VFK Decryption (Optional)

The indexer can decrypt encrypted notes using Viewing Full Keys (VFKs). Each shielded address
has its own VFK, so the indexer supports multiple VFKs via a registry.

### Configuration Options

1. **VFK Config File** (recommended for multiple addresses):
   Set `VFK_CONFIG_FILE` to point to a JSON file:
   ```bash
   VFK_CONFIG_FILE=./vfk_config.json cargo run -p sov-indexer
   ```

2. **Single VFK** (backward compatible):
   ```bash
   AUTHORITY_VFK=fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1 cargo run -p sov-indexer
   ```

### VFK Config File Format

Create a JSON file with your VFKs:

```json
{
  "vfks": [
    {
      "vfk": "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1",
      "shielded_address": "privpool1qypqxpq9qcrsszg2pvxq6rs..."
    },
    {
      "vfk": "a1b2c3d4e5f6789012345678901234567890123456789012345678901234abcd"
    }
  ]
}
```

- `vfk`: Required - the 32-byte VFK as 64 hex characters
- `shielded_address`: Optional - the associated shielded address

### How It Works

Each encrypted note contains an `fvk_commitment` field (a hash of the VFK used to encrypt it).
When indexing, the indexer:
1. Reads the `fvk_commitment` from each encrypted note
2. Looks up the matching VFK in the registry
3. Decrypts the note if a matching VFK is found

This allows the indexer to decrypt notes for multiple addresses, each with their own VFK.

### VFK Registry Table

VFKs are stored in the `vfk_registry` table with columns:
- `fvk_commitment` (primary key): Hash of the VFK for fast lookup
- `vfk`: The actual 32-byte VFK (hex-encoded)
- `shielded_address`: The associated shielded address (optional)

### REST API for VFK Management

You can add/remove VFKs at runtime without restarting the indexer:

**List all VFKs:**
```bash
curl http://localhost:13100/vfks
```

**Add a new VFK:**
```bash
curl -X POST http://localhost:13100/vfks \
  -H "Content-Type: application/json" \
  -d '{
    "vfk": "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1",
    "shielded_address": "privpool1..."
  }'
```

**Add a new VFK with commitment verification:**
```bash
# If you know the expected fvk_commitment, you can provide it to verify
# the VFK is correct before adding it to the registry
curl -X POST http://localhost:13100/vfks \
  -H "Content-Type: application/json" \
  -d '{
    "vfk": "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1",
    "fvk_commitment": "abc123...",
    "shielded_address": "privpool1..."
  }'
```

The API validates:
- VFK format: must be valid hex, exactly 32 bytes (64 hex chars)
- Commitment match: if `fvk_commitment` is provided, it must match the commitment computed from the VFK
- No duplicates: returns 409 Conflict if the VFK is already registered

**Delete a VFK:**
```bash
curl -X DELETE http://localhost:13100/vfks/<fvk_commitment>
```

Changes take effect immediately - new transactions will be decrypted using the updated registry,
and existing indexed deposits/transfers with missing recipients are backfilled from encrypted notes.
The registry uses DashMap for lock-free concurrent access during indexing.

Notes
- On startup, if the DA DB is not ready, the service logs a warning and retries in the background.
- The index DB schema is created automatically on first run.
- VFK decryption only works in `sync` mode (local index database).
- VFKs from config files are persisted to the database for reuse across restarts.
