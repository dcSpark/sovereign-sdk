# sov-midnight-adapter

Utilities for querying the Midnight bridge contract state via the public GraphQL indexer and decoding it into Rust structures.

## Features
- `MidnightIndexerClient` wraps a `reqwest::Client` and exposes a `snapshot()` helper that downloads, deserializes, and analyzes the on-chain contract state.
- `BridgeContractSnapshot` reports the next bridge message index plus a `BTreeMap` of parsed deposits.
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
let snapshot = client.snapshot().await?;
# Ok(())
# }
```
