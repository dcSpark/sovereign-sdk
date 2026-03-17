# L1 Interactions for Rollup Ligero

This guide explains how the TEE rollup interacts with the Midnight L1 Bridge contract, and how to run that flow locally.

## Short Operational Summary

In TEE mode, the rollup periodically aggregates DA-backed execution into batch public data, obtains oracle-backed attestation material, then settles to the Midnight Bridge on L1:

1. `commitBatch(parentBatchHash, batchHash)` records the next batch commitment.
2. `finalizeBatch(batchPublicData, signatures, signerBitmap, finalizeTimestamp)` finalizes that committed batch.

## Where L1 Contract Tooling Lives

- Bridge contract and interaction tooling: `examples/rollup-ligero/midnight-l2-contracts`
- Executor service (HTTP API for the rollup): `examples/rollup-ligero/midnight-l2-contracts/bridge-cli`

## Automated Flow (recommended)

When `[tee_configuration.l1_bridge]` is configured in `rollup_config_tee_local.toml`, the rollup manages the full Bridge lifecycle automatically:

1. **Genesis detection**: On first start (empty ledger DB), the rollup computes the genesis state root.
2. **Auto-deploy**: If no `contract_address` is configured or persisted, the rollup invokes `bridge-cli deploy` as a subprocess, passing the genesis state root and batch hash. The resulting contract address is persisted to `<storage_path>/bridge_contract_address`.
3. **Executor startup**: The rollup spawns the executor service as a managed child process with the correct contract address and network configuration.
4. **Health check**: The rollup waits for the executor to become ready (polls `/state`).
5. **Normal operation**: The TEE manager calls `commitBatch` and `finalizeBatch` via the executor.
6. **Shutdown**: When the rollup exits, the managed executor child process is killed automatically.

On subsequent starts (non-genesis), the rollup loads the persisted contract address and spawns the executor without deploying.

### Prerequisites

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
[sequencer.extension.tee_configuration]
tee_attestation_oracle_url = "http://127.0.0.1:8090"
rollup_id_hex = "0000000000000000000000000000000000000000000000000000000000000000"

[sequencer.extension.tee_configuration.l1_bridge]
bridge_cli_path = "midnight-l2-contracts/bridge-cli"
network = "undeployed"
executor_port = 3001
funding_seed = "0000000000000000000000000000000000000000000000000000000000000001"
# contract_address = ""  # uncomment to skip auto-deploy and use a pre-existing contract
```

### Running

```bash
# Fresh start (wipes state, new genesis, auto-deploys contract):
TEE_RESET=1 ./tee_local.sh --release --skip-build

# Restart (reuses existing state and persisted contract address):
./tee_local.sh --release --skip-build
```

### Disabling L1 Interactions

To run in TEE mode without any L1 interactions, comment out the entire `[sequencer.extension.tee_configuration.l1_bridge]` section. The rollup will operate with local attestations only.

## Manual Flow (legacy)

If you prefer to manage the contract and executor manually (or need to connect to an external executor), omit the `l1_bridge` section and use `executor_url` instead.

### Deploy the Contract Manually

Compute the genesis values:

```bash
cargo run --bin print-genesis-info -- --rollup-config rollup_config_tee_local.toml --genesis-dir demo_data_tee/genesis
```

Deploy in `midnight-l2-contracts/bridge-cli`:

```bash
npm run cli -- deploy -n undeployed --genesis-state-root <genesisStateRoot_> --genesis-batch-hash <genesisBatchHash_>
```

Set `BRIDGE_CONTRACT_ADDRESS` in `bridge-cli/.env`, then start the executor:

```bash
EXECUTOR_PORT=3001 npm run executor
```

### Configure the Rollup

```toml
[sequencer.extension.tee_configuration]
tee_attestation_oracle_url = "http://127.0.0.1:8090"
executor_url = "http://127.0.0.1:3001"
rollup_id_hex = "0000000000000000000000000000000000000000000000000000000000000000"
```

## Log Lines to Watch

- Success:
  - `L1 commitBatch submitted via executor`
  - `L1 finalizeBatch submitted via executor`
  - `L1 Bridge contract deployed successfully`
  - `Executor service is ready`
- Diagnostics/failures:
  - `Executor commit_batch failed; batch cursor will NOT advance`
  - `Executor finalize_batch failed; batch cursor will NOT advance`
  - `Deploying L1 Bridge contract via bridge-cli (this may take a while)...`
  - `No L1 source available; defaulting to batch_index=0`
