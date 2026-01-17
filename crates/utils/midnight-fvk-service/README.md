# Midnight FVK Service

Small REST service that issues fresh Midnight **Full Viewing Keys** (FVKs), returns the Poseidon2 commitment `H("FVK_COMMIT_V1" || fvk)`, and signs that commitment with an `ed25519` key.

The service does **not** allow user-controlled seeds: every request gets a new, server-generated FVK.

## Quickstart

Generate keys (prints `.env` lines):

```bash
cargo run -p midnight-fvk-service -- keygen
```

Run the service (expects env vars; see `.env.example`):

```bash
cargo run -p midnight-fvk-service -- serve
```

The service requires both `MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX` and `MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX` and verifies on startup that the secret key derives the configured public key.

Request an FVK:

```bash
curl -sS -X POST http://127.0.0.1:8088/v1/fvk \
  -H 'content-type: application/json' \
  -d '{}' | jq
```

Example response:

```json
{
  "fvk": "1554138154a8207a161cb73623b1482004c21baf83d0cddcb2dbf950808d7a9b",
  "fvk_commitment": "269a53ca75141d75760b15d41872cce5c0c0c98cf1f402573f049dda7cf1d812",
  "signature": "46011414f9a6bea4326a4f8c81d53347dad1b7aaf1d805d3eca89f51d87379a474b8c54dfb24e58a59673a05a180413a2d0ba5c65bb9d230dafa2537b0b19dff",
  "signer_public_key": "b62f617a96e4332e222be8dbedcbd8efec9fc06ec1f9c302998d37a64cc653f2",
  "signature_scheme": "ed25519",
  "fvk_commitment_scheme": "Poseidon2 H(\"FVK_COMMIT_V1\" || fvk)"
}
```

## Private FVK Lookup (for indexer)

The service can optionally expose a **token-protected** endpoint to look up the private `fvk` by its public `fvk_commitment` (for indexer decryption).

- Configure `MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN` (non-empty) on the service.
- Call with `Authorization: Bearer <token>`.

Example:

```bash
curl -sS "http://127.0.0.1:8088/v1/fvk/<fvk_commitment_hex>" \
  -H "Authorization: Bearer $MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN" | jq
```

## Client Integration (POOL_FVK_PK)

To enforce “pool-signed viewer commitments” in the proof verifier, set `POOL_FVK_PK` to the same value as `MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX` (the 32-byte `ed25519` public key). When enabled, the verifier requires Transfer/Withdraw proofs to carry a signature (`pool_sig_hex`) over the viewer `fvk_commitment`.

`midnight-e2e-benchmarks` fetches viewer FVKs from this service when enforcement is enabled:

- `MIDNIGHT_FVK_SERVICE_URL` (default `http://127.0.0.1:8088`)

## State & Restarts

- Issued FVKs are persisted in SQLite (see `MIDNIGHT_FVK_SERVICE_DB` in `crates/utils/midnight-fvk-service/.env.example`).
- The service keeps a monotonic `index` counter for operational introspection and persists it so restarts continue from the previous value.
- You can force the counter forward on startup with `MIDNIGHT_FVK_SERVICE_LAST_ISSUED_INDEX`; the larger value wins between the DB and ENV.
