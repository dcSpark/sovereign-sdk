# Midnight FVK Service

Small REST service that issues Midnight **Full Viewing Keys** (FVKs), returns the Poseidon2 commitment `H("FVK_COMMIT_V1" || fvk)`, and signs that commitment with a Sovereign-compatible `secp256k1` key.

This is a reference implementation of the “signed unique FVK” flow described in `midnight-docs/explanation_selective_privacy.md`.

## Quickstart

Generate keys (prints `.env` lines):

```bash
cargo run -p midnight-fvk-service -- keygen
```

Run the service (expects env vars; see `.env.example`):

```bash
cargo run -p midnight-fvk-service -- serve
```

Request an FVK:

```bash
curl -sS -X POST http://127.0.0.1:8088/v1/fvk \
  -H 'content-type: application/json' \
  -d '{"seed":"user:alice"}' | jq
```

If you omit `seed`/`seed_hex`, the service generates a random 32-byte seed and returns it as `seed_hex` so the same FVK can be re-derived later.

## State & Restarts

- Issued FVKs are persisted in SQLite (see `MIDNIGHT_FVK_SERVICE_DB` in `crates/utils/midnight-fvk-service/.env.example`).
- If `seed`/`seed_hex` are omitted, the service uses a monotonic `index` counter for the seed (`seed_kind: "index"`), and persists the counter so restarts continue from the previous value.
- You can force the counter forward on startup with `MIDNIGHT_FVK_SERVICE_LAST_ISSUED_INDEX`; the larger value wins between the DB and ENV.
