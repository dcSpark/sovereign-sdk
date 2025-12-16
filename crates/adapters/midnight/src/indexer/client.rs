use graphql_client::reqwest::post_graphql;
use graphql_client::{GraphQLQuery, Response};
use reqwest::{Client, Url};
use thiserror::Error;
use url::ParseError;

/// GraphQL scalar used by Midnight that comes through as a plain hex string.
pub type HexEncoded = String;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/indexer/schema.graphql",
    query_path = "src/indexer/queries/block.graphql",
    response_derives = "Debug, Clone, Deserialize, Serialize",
    scalar = "HexEncoded = String"
)]
pub struct BlockQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/indexer/schema.graphql",
    query_path = "src/indexer/queries/contract_state.graphql",
    response_derives = "Debug, Clone, Deserialize, Serialize",
    scalar = "HexEncoded = String"
)]
pub struct ContractStateQuery;

/// GraphQL client tailored to the Midnight indexer data service.
#[derive(Debug, Clone)]
pub struct IndexerClient {
    endpoint: Url,
    http: Client,
}

impl IndexerClient {
    /// Construct a client using an already parsed `Url` and the default `reqwest::Client`.
    pub fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            http: Client::new(),
        }
    }

    /// Construct a client by parsing the provided endpoint.
    pub fn try_new(endpoint: &str) -> Result<Self, MidnightGraphqlClientBuilderError> {
        let url = Url::parse(endpoint)?;
        Ok(Self::new(url))
    }

    /// Construct a client with a caller-provided HTTP client (useful for custom middleware).
    pub fn with_http_client(endpoint: Url, http: Client) -> Self {
        Self { endpoint, http }
    }

    /// Expose the parsed endpoint for diagnostics or logging.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Fetch a block summary using either a block hash or a block height.
    pub async fn get_block(
        &self,
        offset: Option<block_query::BlockOffset>,
    ) -> Result<Option<block_query::BlockQueryBlock>, MidnightGraphqlClientError> {
        let variables = block_query::Variables { offset };
        let response =
            post_graphql::<BlockQuery, _>(&self.http, self.endpoint.clone(), variables).await?;
        let data = extract_graphql_data(response)?;
        Ok(data.block)
    }

    /// Fetch the latest contract action (including state) for a contract, optionally scoped by offset.
    pub async fn get_contract_state(
        &self,
        address: impl Into<HexEncoded>,
        offset: Option<contract_state_query::ContractActionOffset>,
    ) -> Result<
        Option<contract_state_query::ContractStateQueryContractAction>,
        MidnightGraphqlClientError,
    > {
        let variables = contract_state_query::Variables {
            address: address.into(),
            offset,
        };
        let response =
            post_graphql::<ContractStateQuery, _>(&self.http, self.endpoint.clone(), variables)
                .await?;
        let data = extract_graphql_data(response)?;
        Ok(data.contract_action)
    }
}

fn extract_graphql_data<T>(response: Response<T>) -> Result<T, MidnightGraphqlClientError> {
    if let Some(errors) = response.errors {
        return Err(MidnightGraphqlClientError::Graphql(errors));
    }

    response.data.ok_or(MidnightGraphqlClientError::MissingData)
}

#[derive(Debug, Error)]
pub enum MidnightGraphqlClientError {
    #[error("network request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("graphql response returned errors: {0:?}")]
    Graphql(Vec<graphql_client::Error>),
    #[error("graphql response did not include data")]
    MissingData,
}

#[derive(Debug, Error)]
pub enum MidnightGraphqlClientBuilderError {
    #[error("invalid GraphQL endpoint: {0}")]
    InvalidEndpoint(#[from] ParseError),
}

#[cfg(test)]
mod tests {
    use super::*;

    //const INDEXER_URL: &str = "https://indexer.testnet-02.midnight.network/api/v1/graphql";
    const INDEXER_URL: &str = "http://localhost:32797/api/v1/graphql";

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a running Midnight indexer instance"]
    async fn block_query_smoke_test() {
        let client = IndexerClient::try_new(INDEXER_URL)
            .expect("hardcoded Midnight indexer URL should parse");

        let block = client
            .get_block(None)
            .await
            .expect("block query should succeed when an indexer is running");

        match block {
            Some(block) => {
                println!("Fetched block {:#?}", block);
            }
            None => {
                panic!("indexer returned no block");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a running Midnight indexer instance"]
    async fn contract_state_query_smoke_test() {
        // This needs to be an actual deployed (bridge) contract address.
        const CONTRACT_ADDRESS: &str =
            "0002007d57c06ebb943774a87fe28b7ed6c7a4e176890a9ea12f4c4679afe09ae7c19d";

        let client = IndexerClient::try_new(INDEXER_URL)
            .expect("hardcoded Midnight indexer URL should parse");

        let state = client
            .get_contract_state(CONTRACT_ADDRESS.to_owned(), None)
            .await
            .expect("contract state query should succeed when an indexer is running");

        match state {
            Some(action) => {
                println!("Fetched contract action {action:#?}");
            }
            None => {
                println!("contract not found or has no state");
            }
        }
    }
}
