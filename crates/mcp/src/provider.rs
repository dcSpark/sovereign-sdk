//! RPC Provider for interacting with the Sovereign rollup
//!
//! This module handles all RPC communication with the rollup node, including:
//! - Chain state queries (nonce, balance)
//! - Transaction submission
//! - Fee estimation
//! - Block queries
//! - Schema queries (chain_id, chain_name)

use std::sync::Arc;

use anyhow::{Context, Result};
use sov_bank::TokenId;
use sov_modules_api::{Amount, CryptoSpec, Spec};
use sov_node_client::NodeClient;

/// Chain data from the rollup schema
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChainData {
    /// The chain ID
    pub chain_id: u64,
    /// The chain name
    pub chain_name: String,
}

/// Transaction status from the sequencer
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TransactionStatus {
    /// Transaction hash ID
    pub id: String,
    /// Transaction status (e.g., "pending", "confirmed", "failed")
    pub status: String,
}

/// Provider for RPC communication with the Sovereign rollup
///
/// Responsible for all network communication and chain state queries.
/// Does NOT handle wallet state or key management - that's WalletContext's job.
#[derive(Clone)]
pub struct Provider {
    client: Arc<NodeClient>,
    rpc_url: String,
}

impl Provider {
    /// Create a new provider connected to the given RPC URL
    pub async fn new(rpc_url: &str) -> Result<Self> {
        let client = NodeClient::new(rpc_url)
            .await
            .with_context(|| format!("Failed to connect to rollup node at {}", rpc_url))?;

        Ok(Self {
            client: Arc::new(client),
            rpc_url: rpc_url.to_string(),
        })
    }

    /// Fetch chain data from the rollup schema endpoint
    ///
    /// This queries the `/rollup/schema` endpoint to get chain metadata including
    /// the chain ID and chain name directly from the rollup.
    ///
    /// # Returns
    /// Chain data containing chain_id and chain_name
    ///
    /// # Example
    /// ```rust,no_run
    /// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
    /// let chain_data = provider.get_chain_data().await?;
    /// println!("Chain ID: {}", chain_data.chain_id);
    /// println!("Chain Name: {}", chain_data.chain_name);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_chain_data(&self) -> Result<ChainData> {
        let schema_url = format!("{}/rollup/schema", self.rpc_url);

        tracing::debug!("Fetching chain data from: {}", schema_url);

        let response = reqwest::get(&schema_url)
            .await
            .with_context(|| format!("Failed to fetch schema from {}", schema_url))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Schema endpoint returned error status: {}",
                response.status()
            );
        }

        let schema_json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse schema JSON")?;

        let chain_data_json = schema_json
            .get("schema")
            .and_then(|s| s.get("chain_data"))
            .context("Schema missing 'chain_data' field")?;

        let chain_data: ChainData = serde_json::from_value(chain_data_json.clone())
            .context("Failed to deserialize chain_data")?;

        tracing::debug!(
            "Fetched chain data - ID: {}, Name: {}",
            chain_data.chain_id,
            chain_data.chain_name
        );

        Ok(chain_data)
    }

    /// Get the balance for a specific address and token
    pub async fn get_balance<S: Spec>(
        &self,
        address: &S::Address,
        token_id: &TokenId,
    ) -> Result<Amount> {
        self.client
            .get_balance::<S>(address, token_id, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to get balance for token {} at address {:?}",
                    token_id, address
                )
            })
    }

    /// Get the nonce for a public key
    pub async fn get_nonce<S: Spec>(
        &self,
        public_key: &<S::CryptoSpec as CryptoSpec>::PublicKey,
    ) -> Result<u64> {
        self.client
            .get_nonce_for_public_key::<S>(public_key)
            .await
            .context("Failed to get nonce from rollup")
    }

    /// Submit a raw transaction to the rollup sequencer
    ///
    /// This method accepts a borsh-serialized `Transaction` and submits it to the sequencer.
    /// Returns the transaction hash from the rollup.
    pub async fn submit_transaction(&self, raw_tx: Vec<u8>) -> Result<String> {
        let tx_hashes = self
            .client
            .send_transactions_to_sequencer(vec![raw_tx], false)
            .await
            .context("Failed to submit transaction to sequencer")?;

        let tx_hash = tx_hashes
            .first()
            .ok_or_else(|| anyhow::anyhow!("No transaction hash returned from sequencer"))?;
        Ok(tx_hash.to_string())
    }

    /// Get the status of a transaction by its hash
    ///
    /// This queries the `/sequencer/txs/{txHash}/status` endpoint to check
    /// if a transaction has been received and what its current status is.
    ///
    /// # Parameters
    /// * `tx_hash` - The transaction hash (with or without 0x prefix)
    ///
    /// # Returns
    /// Transaction status containing the ID and status string
    ///
    /// # Example
    /// ```rust,no_run
    /// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
    /// let status = provider.get_transaction_status("0x1234...").await?;
    /// println!("Transaction status: {}", status.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_transaction_status(&self, tx_hash: &str) -> Result<TransactionStatus> {
        // Remove 0x prefix if present
        let tx_hash_clean = tx_hash.to_string(); // .strip_prefix("0x").unwrap_or(tx_hash);

        let status_url = format!("{}/sequencer/txs/{}/status", self.rpc_url, tx_hash_clean);

        tracing::debug!("Fetching transaction status from: {}", status_url);

        let response = reqwest::get(&status_url)
            .await
            .with_context(|| format!("Failed to fetch transaction status from {}", status_url))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Transaction status endpoint returned error status: {}",
                response.status()
            );
        }

        let tx_status: TransactionStatus = response
            .json()
            .await
            .context("Failed to parse transaction status JSON")?;

        tracing::debug!("Transaction {} status: {}", tx_status.id, tx_status.status);

        Ok(tx_status)
    }

    /// Get the RPC URL this provider is connected to
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Check if the rollup is healthy and responding
    ///
    /// Queries the `/healthcheck` endpoint to verify the rollup is available.
    #[allow(dead_code)]
    pub async fn is_healthy(&self) -> bool {
        let health_url = format!("{}/healthcheck", self.rpc_url);

        match reqwest::get(&health_url).await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }
}
