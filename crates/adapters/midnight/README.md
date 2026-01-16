# sov-midnight-adapter

Utilities for querying the Midnight bridge contract state via the public GraphQL indexer and decoding it into Rust structures.

## Features
- `MidnightIndexerClient` wraps a `reqwest::Client` and exposes a `snapshot()` helper that downloads, deserializes, and returns the on-chain bridge `BridgeLedger` in one call.
- `BridgeLedger`, `RollupLedger`, `L2GatewayLedger`, `L2MessengerLedger`, and `L2MessageQueueLedger` mirror every section of the bridge state so you can introspect the data without reimplementing the decoder logic.
- `MidnightDeposit` mirrors the on-chain tuple layout so downstream crates can convert it into their own types.

## Live indexer test
The crate ships with an integration-style test that can hit a real Midnight indexer. To run it:

1. Export the indexer HTTP endpoint and bridge contract address:
   ```bash
   export MIDNIGHT_INDEXER_ENDPOINT=https://indexer.preview.midnight.network/api/v3/graphql
   export MIDNIGHT_CONTRACT_ADDRESS=fa8533250190a9d2b39686523e7b13e7dc30647a341f8163dceaec2cdc365f12
   ```
2. Execute the test:
   ```bash
   cargo test -p sov-midnight-adapter fetches_real_snapshot_when_configured -- --nocapture
   ```

If either variable is missing or empty, the test logs a skip message and exits successfully, keeping `cargo test` runs lightweight by default.

## Usage
```rust
use reqwest::Client;
use sov_midnight_adapter::MidnightIndexerClient;

# async fn example() -> anyhow::Result<()> {
let http = Client::builder().build()?;
let client = MidnightIndexerClient::new(http, endpoint, contract_address);
let ledger = client.snapshot().await?;

let cursor = ledger.rollup.next_cross_domain_message_index;
let deposits = &ledger.rollup.l1_to_l2_deposits;

let owner = ledger.rollup.owner;
let fee_vault = ledger.rollup.fee_vault;
let gateway_balances = &ledger.l2_gateway.balances;
# Ok(())
# }
```
