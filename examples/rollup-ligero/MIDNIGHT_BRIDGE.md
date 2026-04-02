# Midnight Bridge for Rollup Ligero

The Midnight Bridge is a configurable component of the rollup. When `[sequencer.extension.midnight_bridge]` is present in the rollup config TOML, the rollup will run the following types of Midnight L1 network interactions:

## Midnight Bridge responsibilities - overview

### 1. L1 Bridge contract deployment

If `contract_address` is not provided in config and the rollup is starting from genesis, it will deploy a new Bridge contract instance to the configured Midnight network.

### 2. Settling batches on L1

In TEE mode, the rollup periodically aggregates DA-backed execution into batch public data, obtains oracle-backed attestation material, then settles to the Midnight Bridge on L1:

1. `commitBatch(parentBatchHash, batchHash)` records the next batch commitment.
2. `finalizeBatch(batchPublicData, signatures, signerBitmap, finalizeTimestamp)` finalizes that committed batch.

### 3. Bridging - deposits

TODO: extend with a bit of extra detail to look more like sections 2. and 4.

The rollup observes the L1 Bridge contract for deposit events where NIGHT token is locked on the contract. Each such event comes with an L2 (rollup) bridging recipient address to which the rollup credits the appropriate bridged funds.

### 4. Bridging - withdrawals

The other leg of the bridge: moving NIGHT from L2 back to L1.

1. A user submits a `WithdrawNight` transaction on L2, which burns L2 NIGHT and appends the withdrawal to a 16-level incremental Merkle tree inside the `MidnightWithdrawals` STF module.
2. Each finalized batch attests the cumulative `withdrawRoot` to the L1 Bridge contract via `finalizeBatch`.
3. The bridge worker automatically detects pending withdrawals and relays their Merkle proofs to L1 by calling the executor's `POST /relay-withdraw-night-with-proof` endpoint.
4. Once relayed, the recipient can claim funds on L1 via `claimL1WithdrawalUnshielded`.

## Testing

### Prerequisites

The repository (or the `feature/midnight-bridge` branch) needs to be checked ot with submodules:

```bash
git clone --branch feature/midnight-bridge --recurse-submodules https://github.com/dcSpark/sovereign-sdk.git
# or, if already cloned:
git checkout feature/midnight-bridge
git submodule update --init --recursive
```

Bridge contract and interaction tooling now lives in `examples/rollup-ligero/midnight-l2-contracts`.

The local Midnight L1 network must already be running before starting the rollup:

```bash
cd examples/rollup-ligero/midnight-l2-contracts
npm install
npm run build
npm run setup-standalone
```

This starts the Midnight node, indexer, and proof server via Docker.

### Configuration

In `rollup_config_tee_local.toml`:

```toml
[sequencer.extension.midnight_bridge]
bridge_cli_path = "midnight-l2-contracts/bridge-cli"
network = "undeployed"
executor_port = 3001
funding_seed = "0000000000000000000000000000000000000000000000000000000000000001"
rollup_id_hex = "0000000000000000000000000000000000000000000000000000000000000000"
indexer_http = "http://localhost:8088/api/v3/graphql"
indexer_timeout_ms = 30000
# contract_address = ""        # auto-deployed on genesis if omitted
signing_key_path = "assets/midnight_bridge_signer.json"
poll_interval_ms = 1000
max_fee = 1000000

[sequencer.extension.tee_configuration]
tee_attestation_oracle_url = "http://127.0.0.1:8090"
```

### Running

```bash
# Fresh start (wipes state, new genesis, auto-deploys contract):
TEE_RESET=1 ./tee_local.sh --release --skip-build

# Restart (reuses existing state and persisted contract address):
./tee_local.sh --release --skip-build
```

### Making deposits

TODO

### Making withdrawals

**Step 1 — Initiate a withdrawal on L2** (burns NIGHT, enqueues a message):

```bash
# Create the call message (use your own midnight_address):
cat > withdraw_night.json << 'EOF'
{
  "withdraw_night": {
    "midnight_address": "1e524a8e02b8022f243db6c992f48c866dff4fa3b1c01624f9f2c1d269e11017",
    "amount": "1000000000000",
    "gas_limit": null
  }
}
EOF

# Build sov-cli if needed (must use rollup-ligero's, not demo-rollup's — they
# link different ed25519 implementations and signatures are incompatible):
# cargo build -p sov-rollup-ligero --bin sov-cli --release

# One-time setup: point sov-cli at the rollup and import a genesis-funded key.
rm ~/.sov_cli_wallet/wallet_state.json
python3 -c "import json; json.dump(json.load(open('demo_data_tee/genesis/generated_keypairs.json'))[0], open('/tmp/user_key.json','w'))"
../../target/release/sov-cli node set-url http://127.0.0.1:12346
../../target/release/sov-cli keys import --path /tmp/user_key.json --nickname user

# Import and submit the withdrawal:
../../target/release/sov-cli transactions import from-file midnight-withdrawals \
  --path withdraw_night.json \
  --max-fee 1000000000
../../target/release/sov-cli node submit-batch by-nickname user
```

**Step 2 — Verify the withdrawal was queued:**

```bash
curl -s http://127.0.0.1:12346/modules/midnight-withdrawals/withdrawals/queue | jq
# → {"next_nonce": 1, "withdraw_root_hex": "..."}
```

**Step 3 — Wait for automatic relay.** After the next batch is finalized on L1, the bridge worker relays the withdrawal proof automatically. You can inspect the proof at any time:

```bash
curl -s http://127.0.0.1:12346/modules/midnight-withdrawals/withdrawals/0/proof?batch_index=1 | jq
```

**Step 4 — Claim on L1.** Once the proof has been relayed, the recipient claims funds via the executor:

```bash
curl -s -X POST http://127.0.0.1:3001/claim-l1-withdrawal-unshielded \
  -H 'Content-Type: application/json' \
  -d '{
    "recipient": "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
    "amount": "1000000"
  }' | jq
```

### Disabling L1 Interactions

To run without any L1 interactions, comment out the entire `[sequencer.extension.midnight_bridge]` section. The rollup will operate without bridge deployment, executor, or deposit monitoring.

## Operation details

When `[sequencer.extension.midnight_bridge]` is configured in the rollup config, the rollup manages the full Bridge lifecycle automatically:

1. **Genesis detection**: On first start (empty ledger DB), the rollup computes the genesis state root.
2. **Auto-deploy**: If no `contract_address` is configured or persisted, the rollup invokes `bridge-cli deploy` as a subprocess, passing the genesis state root and batch hash. The resulting contract address is persisted to `<storage_path>/bridge_contract_address`.
3. **Executor startup**: The rollup spawns the executor service as a managed child process with the correct contract address and network configuration.
4. **Health check**: The rollup waits for the executor to become ready (polls `/state`).
5. **Normal operation**: The TEE manager calls `commitBatch` and `finalizeBatch` via the executor. The deposit monitor polls the indexer for new deposits.
6. **Shutdown**: When the rollup exits, the managed executor child process is killed automatically.

On subsequent starts (non-genesis), the rollup loads the persisted contract address and spawns the executor without deploying.
