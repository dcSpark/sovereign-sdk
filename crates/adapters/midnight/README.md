# `midnight`

Implements interfacing with the Midnight L1 blockchain.

## Development

### Indexer

Source of [GraphQL schema](./src/indexer/schema.graphql) is at [midnight-indexer](https://github.com/midnightntwrk/midnight-indexer/blob/main/indexer-api/graphql/schema-v3.graphql).

### Manual smoke test

To run the ignored client smoke test against a locally running Midnight indexer (listening on `http://127.0.0.1:9988/graphql`), execute:

```sh
cargo test -p midnight block_query_smoke_test -- --ignored --nocapture
```

_TODO: add proper automated integration tests once a stable local indexer fixture is available._