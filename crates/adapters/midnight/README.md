# `midnight`

Implements interfacing with the Midnight L1 blockchain.

## Development

### Indexer

Source of [GraphQL schema](./src/indexer/schema.graphql) is at [midnight-indexer](https://github.com/midnightntwrk/midnight-indexer/blob/main/indexer-api/graphql/schema-v3.graphql).

### Manual smoke tests

To run the ignored client smoke tests against a locally running Midnight indexer, execute:

```sh
cargo test -p midnight --lib -- --ignored --nocapture
```

_TODO: add proper automated integration tests once a stable local indexer fixture is available._

### Debug

#### Query indexer state

Query a block:
```
curl -s \
  -H 'content-type: application/json' \
  -d @- \
  http://localhost:32794/api/v1/graphql <<'JSON'
{
  "query": "query { block(offset: { height: 33 }) { hash height protocolVersion timestamp transactions { hash } } }" 
}
JSON
```

Query a transaction:
```
curl -s \
  -H 'content-type: application/json' \
  -d @- \
  http://localhost:32797/api/v1/graphql <<'JSON'
{
  "query": "query Tx($hash: HexEncoded!) { transactions(offset: { hash: $hash }) { hash contractActions { __typename ... on ContractDeploy { address state chainState } } } }",
  "variables": {
    "hash": "2bf045dbf89d481b669efdee51cc3a215f462898d612ab3ae17c1a4fac91ea02"
  }
}
JSON
```

Query a contract:
```
curl -s \
  -H 'content-type: application/json' \
  -d @- \
  http://localhost:32797/api/v1/graphql <<'JSON'
{
  "query": "query ContractStateQuery($address: HexEncoded!, $offset: ContractActionOffset) { contractAction(address: $address, offset: $offset) { __typename ... on ContractDeploy { address state chainState } ... on ContractCall { address state chainState entryPoint } ... on ContractUpdate { address state chainState } } }",
  "variables": {
    "address": "0002007d57c06ebb943774a87fe28b7ed6c7a4e176890a9ea12f4c4679afe09ae7c19d",
    "offset": null
  }
}
JSON
```
