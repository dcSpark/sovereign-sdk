# Proof Verifier Service

Off-chain parallel proof verification service for the Ligero rollup.

## Overview

This service provides a high-throughput, parallel proof verification layer that sits between clients and the rollup node. It:

1. **Receives signed Ligero transactions** (same format as node RPC)
2. **Verifies signatures** (deserialized transaction integrity)
3. **Verifies Ligero proofs in parallel** (using tokio async tasks)
4. **Transforms to non-ZK transactions** (value-setter without proofs)
5. **Submits to the rollup node** (if verification succeeds)

```
┌─────────┐                    ┌──────────────────┐                    ┌──────────┐
│ Client  │ ─── ZK TX ────────>│  Verifier        │ ─── Non-ZK TX ───>│   Node   │
│         │    (3.2 MB)        │  Service         │    (100 bytes)    │   RPC    │
└─────────┘                    │                  │                    └──────────┘
                               │  • Verify sig    │
                               │  • Verify proof  │
                               │  • Parallel!     │
                               └──────────────────┘
```

## Benefits

### **Parallel Verification** 🚀

- Process multiple proofs concurrently using Tokio async tasks
- Configurable concurrency limit (default: number of CPU cores)
- **NOT limited by sequential transaction processing** on the node

### **Off-Chain Computation** 💰

- Expensive proof verification happens off-chain
- Node only processes lightweight non-ZK transactions
- **~74M gas per TX → ~5K gas per TX** (15,000x reduction!)

### **Improved TPS** ⚡

- Node processes non-ZK TXs much faster (~25ms vs 537ms)
- Verification happens in parallel, not sequentially
- **Expected TPS increase: 20x+**

## Architecture

### Traditional Flow (Sequential):

```
TX 1 → Node → Verify Sig (235ms) → Verify Proof (300ms) → Execute ─┐
TX 2 → Node → Verify Sig (235ms) → Verify Proof (300ms) → Execute ─┤
TX 3 → Node → Verify Sig (235ms) → Verify Proof (300ms) → Execute ─┼─> ~1.58 tx/s
...                                                                  │
TX 14 → Node → Verify Sig (235ms) → Verify Proof (300ms) → Execute ┘

Total: 14 × 537ms = 7.5s (if fully sequential)
Actual: ~1.07s (with pipelining) = ~13 tx/s max
```

### With Verifier Service (Parallel):

```
TX 1 ─┐
TX 2 ─┤
TX 3 ─┼─> Verifier Service (Parallel) ─┐
...   │   • Verify Sig (235ms)          │
TX 14 ┘   • Verify Proof (300ms)        ├─> Non-ZK TXs → Node ─> ~200+ tx/s
          • Transform (1ms)              │   (25ms each)
          • All in parallel!             ┘

Verifier: 14 TXs × 535ms / 10 workers ≈ 750ms
Node: 14 TXs × 25ms = 350ms
Total: ~1.1s for 14 TXs = ~12.7 tx/s

But with 100 TXs:
Verifier: 100 TXs × 535ms / 10 workers = 5.35s
Node: 100 TXs × 25ms = 2.5s  
Total: ~7.85s for 100 TXs = ~12.7 tx/s (consistent!)
```

## Usage

### Build

```bash
cd crates/utils/sov-proof-verifier-service
cargo build --release
```

### Lambda Container

This service can also run as an AWS Lambda container image via the AWS Lambda Web Adapter.

The repo Dockerfile now exposes a Lambda-compatible image target:

```bash
docker buildx build \
  --platform linux/amd64 \
  --target lambda-runtime \
  -f crates/utils/sov-proof-verifier-service/Dockerfile \
  -t proof-verifier-lambda:latest \
  .
```

To deploy it to AWS with the `midnight` profile:

```bash
AWS_PROFILE=<profile> \
AWS_REGION=<region> \
FUNCTION_NAME=<lambda-function-name> \
ECR_REPOSITORY=<ecr-repository-name> \
ROLE_NAME=<iam-role-name> \
ARCHITECTURE=x86_64 \
PLATFORM=linux/amd64 \
MEMORY_SIZE=10240 \
TIMEOUT=900 \
EPHEMERAL_STORAGE_MB=4096 \
AUTH_TYPE=NONE \
FUNCTION_URL_INVOKE_MODE=RESPONSE_STREAM \
NIGHTSTREAM_REF=<nightstream-git-ref> \
NODE_RPC_URL=<rollup-rpc-url> \
DA_DB=<database-url> \
BIND_ADDR=0.0.0.0:8080 \
LAMBDA_SUBNET_IDS=<subnet-id-1>,<subnet-id-2> \
LAMBDA_SECURITY_GROUP_IDS=<sg-id-1>,<sg-id-2> \
IMAGE_TAG=<image-tag> \
./scripts/deploy_proof_verifier_lambda.sh
```

Optional:

```bash
POOL_FVK_PK=1ecf7f45dd35e4edc0e09205804211d753725bf7b13c54dd5f98f8e9bfec6abc \
./scripts/deploy_proof_verifier_lambda.sh
```

Notes:

- The Lambda image target uses the AWS Lambda Web Adapter and routes traffic to the service on port `8080`.
- `AUTH_TYPE` must now be specified explicitly.
- `FUNCTION_URL_INVOKE_MODE` should be `RESPONSE_STREAM` for `/prove`, so long proofs and large proof payloads can complete over the public Lambda URL.
- The deploy script creates the ECR repository and Lambda execution role if they do not already exist.
- The deploy script requires all deployment-specific identifiers, networking settings, and URLs to be provided explicitly.
- The deploy script accepts `POOL_FVK_PK` as an optional deployment-time configuration override.
- The deploy script configures the Lambda Web Adapter for response streaming.
- `/prove` and `/verify` can run in Lambda without a colocated rollup node. Endpoints that submit to the sequencer still need a reachable `NODE_RPC_URL`.

### Run the Service

```bash
# Default configuration:
# - Local Ligero prover/verifier daemon pools
# - Worker count = number of CPU cores
./target/release/proof-verifier

# Optional: route verification through an external ligero-http-server
./target/release/proof-verifier \
    --bind 0.0.0.0:8080 \
    --node-rpc-url http://127.0.0.1:12346 \
    --prover-service-url http://localhost:8080 \
    --signing-key-path ../test-data/keys/token_deployer_private_key.json \
    --method-id 0x1234... \
    --midnight-method-id 0xabcd... \
    --max-concurrent 10 \
    --log-level debug
```

### Environment Variables

```bash
# Note: these are primarily for Docker/docker-entrypoint.sh wrappers.
# The binary itself uses CLI flags.
export BIND_ADDRESS="127.0.0.1:8080"
export NODE_RPC_URL="http://127.0.0.1:12346"
export SIGNING_KEY_PATH="../test-data/keys/token_deployer_private_key.json"
export METHOD_ID="0x..."
export MIDNIGHT_METHOD_ID="0x..."
# Optional override; if unset, defaults to CPU core count.
export MAX_CONCURRENT_VERIFICATIONS="5"
# Optional: use external ligero-http-server instead of local daemon pools.
export PROVER_SERVICE_URL="http://127.0.0.1:8080"
export LOG_LEVEL="info"
# Optional: enforce pool-signed viewing + require ciphertext bytes for Transfer/Withdraw
export POOL_FVK_PK="0x<32-byte-ed25519-public-key-hex>"

./target/release/proof-verifier
```

### Pool viewing enforcement (`POOL_FVK_PK`)

When `POOL_FVK_PK` is set:

- Transfer/Withdraw proof args must include a pool signature over the viewer `fvk_commitment`.
- Transfer/Withdraw transaction bodies must include `view_ciphertexts` (the actual encrypted payload `ct`), with one ciphertext per output commitment and `fvk_commitment` matching the signed viewer commitment.

Note: `view_attestations` only contains `ct_hash`/`mac` bindings; the ciphertext bytes live in `view_ciphertexts` (and are stored in the worker DB as `encrypted_notes_json`).

## API Endpoints

### POST `/value-setter-zk`

Verify a signed value-setter-zk transaction and submit a transformed non-ZK transaction.

**Request:**
```json
{
  "body": "base64-encoded-signed-transaction"
}
```

**Response:**
```json
{
  "success": true,
  "tx_hash": "0xabc123...",
  "metrics": {
    "deserialize_ms": 1.2,
    "signature_verify_ms": 234.5,
    "proof_verify_ms": 295.3,
    "tx_creation_ms": 0.8,
    "node_submit_ms": 12.1,
    "total_ms": 543.9
  }
}
```

### POST `/midnight-privacy`

Verify signed midnight-privacy transactions (deposit/transfer/withdraw/admin ops), persist verified metadata, and optionally submit to sequencer immediately.

**Request:**
```json
{
  "body": "base64-encoded-signed-transaction"
}
```

### POST `/midnight-privacy/flush`

Flush pending worker-verified transactions to the sequencer when `--defer-submission` is enabled.

### POST `/prove`

Generate a proof with the local daemon prover pool. Response shape is compatible with `ligero-http-server`.
Set `binary: true` to receive raw proof bytes instead of JSON/base64.

**Request:**
```json
{
  "circuit": "note_spend_guest",
  "args": [{ "i64": 1 }, { "i64": 2 }],
  "privateIndices": [1],
  "packing": 8192,
  "gzip": false,
  "binary": false
}
```

**Response (`binary=false`, default):**
```json
{
  "success": true,
  "exitCode": 0,
  "proof": "base64-proof-bytes"
}
```

**Response (`binary=true`):**
- HTTP body is raw proof bytes
- `Content-Type: application/octet-stream`

### POST `/verify`

Verify a proof with the local daemon verifier pool. Response shape is compatible with `ligero-http-server`.

**Request:**
```json
{
  "circuit": "note_spend_guest",
  "args": [{ "i64": 1 }, { "i64": 2 }],
  "privateIndices": [1],
  "proof": "base64-proof-bytes"
}
```

**Response:**
```json
{
  "success": false,
  "exitCode": 1,
  "error": "Verification failed: ..."
}
```

### GET `/health`

Health check endpoint.

**Response:**
```json
{
  "status": "healthy",
  "service": "proof-verifier"
}
```

## Testing

### Modified TPS Test Script

Use the modified test script that sends to the verifier service:

```bash
cd examples/rollup-ligero

# Start the rollup node
cargo run &

# Start the verifier service
cd ../../crates/utils/sov-proof-verifier-service
cargo run --release &

# Run the TPS test (sends to verifier service)
cd ../../../examples/rollup-ligero
./test_tps_with_verifier.sh 50 1 10
```

### Manual Testing

```bash
# 1. Generate a proof
This repo no longer ships a `generate_value_proof` example. Generate a proof using your own host
wrapper (or the module tooling) and submit the resulting `LigeroProofPackage` bytes.

# 2. Sign the transaction
cd ../../..
./target/debug/sov-cli transactions sign \
    --generation 0 \
    --key-nickname DANGER__DO_NOT_USE_WITH_REAL_MONEY \
    --json-output \
    from-file value-setter-zk \
    --max-fee 100000000000 \
    --path crates/adapters/ligero/value_tx.json \
    | jq -r '.signed_tx' \
    | xxd -r -p \
    | base64 > signed_tx.b64

# 3. Submit to verifier service
curl -X POST http://127.0.0.1:8080/value-setter-zk \
    -H "Content-Type: application/json" \
    -d "{\"body\": \"$(cat signed_tx.b64)\"}"
```

## Performance Comparison

### Scenario: 50 Transactions

| Configuration | Method | TPS | Time |
|---------------|--------|-----|------|
| **Direct to Node** | Sequential processing | 1.58 tx/s | 31.6s |
| **Via Verifier (10 workers)** | Parallel verification | ~12.7 tx/s* | ~3.9s |
| **Speedup** | | **8x** | **8x faster** |

\* Estimated based on parallel verification time

### Bottleneck Analysis

**Direct to Node:**
- Bottleneck: Sequential TX processing (537ms each)
- Limit: ~2 tx/s maximum (with perfect batching)

**Via Verifier Service:**
- Bottleneck: Parallel proof verification (535ms / 10 workers)
- Limit: ~18 tx/s maximum (limited by worker count)
- Node processing: ~200+ tx/s (non-ZK TXs are fast!)

## Implementation Status

### ✅ Implemented:

- [x] Axum-based REST API
- [x] Parallel request handling with Tokio
- [x] Semaphore-based concurrency control
- [x] Health check endpoint
- [x] Request/response types
- [x] Ligero proof verification integration
- [x] Error handling and metrics

### ⚠️ TODO (Placeholders):

- [ ] **Signature verification** (currently skipped)
  - Need to deserialize full Transaction<C> structure
  - Verify ECDSA signature against payload hash
  - Copy logic from `sov-modules-api::transaction::Authenticator`
  
- [ ] **Transaction parsing** (currently simplified)
  - Deserialize full signed transaction
  - Extract runtime call and module
  - Parse value-setter-zk call message
  
- [ ] **Transaction signing** (currently placeholder)
  - Load private key from `signing_key_path`
  - Create Transaction<C> for value-setter module
  - Sign with proper nonce and gas
  
- [ ] **Node submission** (currently placeholder)
  - Properly serialize and base64-encode transaction
  - Submit to `/sequencer/txs` endpoint
  - Handle HTTP errors and retries

### 🎯 Next Steps:

1. **Implement signature verification** (copy from node)
2. **Implement transaction parsing** (use sov-modules-api types)
3. **Implement proper transaction signing** (use sov-cli logic)
4. **Implement node submission** (HTTP POST to /sequencer/txs)
5. **Add caching** (cache verified proofs to avoid re-verification)
6. **Add monitoring** (Prometheus metrics for verification times)
7. **Add rate limiting** (protect against DoS)

## Development

### Running Tests

```bash
cargo test -p sov-proof-verifier-service
```

### Checking Logs

```bash
# Info level (default)
RUST_LOG=info cargo run

# Debug level
RUST_LOG=debug cargo run

# Trace level (very verbose)
RUST_LOG=trace cargo run
```

### Performance Profiling

```bash
# With flamegraph
cargo flamegraph --bin proof-verifier

# With perf
cargo build --release
perf record --call-graph dwarf ./target/release/proof-verifier
perf report
```

## Security Considerations

### Trust Model

- **Verifier service is trusted**: Clients must trust it to verify proofs correctly
- **Not suitable for trustless scenarios**: Use on-chain verification for trustless setups
- **Good for**:
  - Private rollups
  - Permissioned systems
  - Development/testing
  - Off-chain proof batching before aggregation

### Attack Vectors

1. **Malicious verifier**: Could accept invalid proofs
   - Mitigation: Run your own verifier service
   
2. **DoS attacks**: Flood with invalid proofs
   - Mitigation: Rate limiting, authentication
   
3. **Replay attacks**: Resubmit old transactions
   - Mitigation: Nonce tracking (already handled by node)

## License

MIT OR Apache-2.0
