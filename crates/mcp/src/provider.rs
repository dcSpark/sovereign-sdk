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
use borsh::{self, BorshDeserialize};
use demo_stf::runtime::Runtime as DemoRuntime;
use midnight_privacy::CallMessage as MidnightCallMessage;
use sov_address::MultiAddressEvm;
use sov_bank::TokenId;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::{
    configurable_spec::ConfigurableSpec, execution_mode::Native,
    transaction::{Transaction, Version0, VersionedTx}, Amount, CryptoSpec, SafeVec, Spec,
};
use sov_node_client::NodeClient;
use sov_nightstream_adapter::Nightstream;

use crate::nightstream::ProofRef;

type McpSpec = ConfigurableSpec<MockDaSpec, Nightstream, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = DemoRuntime<McpSpec>;
type McpTransaction = Transaction<McpRuntime, McpSpec>;

/// Chain data from the rollup schema
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChainData {
    /// The chain ID
    pub chain_id: u64,
    /// The chain name
    pub chain_name: String,
}

/// Transaction involvement item from the indexer
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct InvolvementItem {
    /// Transaction hash
    pub tx_hash: String,
    /// Timestamp in milliseconds
    pub timestamp_ms: i64,
    /// Transaction kind (e.g., "deposit", "withdraw", "transfer")
    pub kind: String,
    /// Sender address (if available)
    pub sender: Option<String>,
    /// Recipient address (if available)
    pub recipient: Option<String>,
    /// Privacy sender address (if available)
    #[serde(default)]
    pub privacy_sender: Option<String>,
    /// Privacy recipient address (if available)
    #[serde(default)]
    pub privacy_recipient: Option<String>,
    /// Transaction amount (if available)
    pub amount: Option<String>,
    /// Anchor root for privacy transactions
    pub anchor_root: Option<String>,
    /// Nullifier for privacy transactions
    pub nullifier: Option<String>,
    /// View Full Viewing Keys (FVKs) for note decryption
    #[serde(default)]
    pub view_fvks: Option<serde_json::Value>,
    /// View attestations for privacy proofs
    #[serde(default)]
    pub view_attestations: Option<serde_json::Value>,
    /// Transaction events from the rollup
    #[serde(default)]
    pub events: Option<serde_json::Value>,
    /// Transaction status (e.g., "Success", "Failed")
    #[serde(default)]
    pub status: Option<String>,
    /// Encrypted notes for privacy transactions
    #[serde(default)]
    pub encrypted_notes: Option<serde_json::Value>,
    /// Decrypted notes for privacy transactions (when VFK is provided)
    #[serde(default)]
    pub decrypted_notes: Option<serde_json::Value>,
    /// Full transaction payload
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// Response from the indexer's list transactions endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ListTransactionsResponse {
    /// List of transaction items
    pub items: Vec<InvolvementItem>,
    /// Cursor for pagination (optional)
    pub next: Option<String>,
    /// Total number of matching transactions (optional)
    #[serde(default)]
    pub total: Option<u64>,
}

fn strip_midnight_proof_from_signed_tx(raw_tx: &[u8]) -> Result<Vec<u8>> {
    let tx: McpTransaction =
        BorshDeserialize::try_from_slice(raw_tx).context("Failed to deserialize signed transaction")?;

    match &tx.versioned_tx {
        VersionedTx::V0(v0) => {
            let runtime_call = match &v0.runtime_call {
                demo_stf::runtime::RuntimeCall::MidnightPrivacy(midnight_call) => {
                    let stripped_call = match midnight_call {
                        MidnightCallMessage::Transfer {
                            anchor_root,
                            nullifiers,
                            view_ciphertexts,
                            gas,
                            ..
                        } => MidnightCallMessage::Transfer {
                            proof: SafeVec::new(),
                            anchor_root: *anchor_root,
                            nullifiers: nullifiers.clone(),
                            view_ciphertexts: view_ciphertexts.clone(),
                            gas: gas.clone(),
                        },
                        MidnightCallMessage::Withdraw {
                            anchor_root,
                            nullifier,
                            withdraw_amount,
                            to,
                            view_ciphertexts,
                            gas,
                            ..
                        } => MidnightCallMessage::Withdraw {
                            proof: SafeVec::new(),
                            anchor_root: *anchor_root,
                            nullifier: *nullifier,
                            withdraw_amount: *withdraw_amount,
                            to: to.clone(),
                            view_ciphertexts: view_ciphertexts.clone(),
                            gas: gas.clone(),
                        },
                        _ => return Ok(raw_tx.to_vec()),
                    };

                    demo_stf::runtime::RuntimeCall::MidnightPrivacy(stripped_call)
                }
                _ => return Ok(raw_tx.to_vec()),
            };

            let lightweight_tx = McpTransaction {
                versioned_tx: VersionedTx::V0(Version0 {
                    signature: v0.signature.clone(),
                    pub_key: v0.pub_key.clone(),
                    runtime_call,
                    uniqueness: v0.uniqueness.clone(),
                    details: v0.details.clone(),
                }),
            };

            borsh::to_vec(&lightweight_tx)
                .context("Failed to serialize proof-stripped signed transaction")
        }
    }
}

/// Provider for RPC communication with the Sovereign rollup
///
/// Responsible for all network communication and chain state queries.
/// Does NOT handle wallet state or key management - that's WalletContext's job.
#[derive(Clone)]
pub struct Provider {
    client: Arc<NodeClient>,
    rpc_url: String,
    verifier_url: String,
    indexer_url: String,
    http_client: reqwest::Client,
}

impl Provider {
    /// Create a new provider connected to the given RPC URL, verifier service, and indexer
    pub async fn new(rpc_url: &str, verifier_url: &str, indexer_url: &str) -> Result<Self> {
        let client = NodeClient::new(rpc_url)
            .await
            .with_context(|| format!("Failed to connect to rollup node at {}", rpc_url))?;

        Ok(Self {
            client: Arc::new(client),
            rpc_url: rpc_url.to_string(),
            verifier_url: verifier_url.to_string(),
            indexer_url: indexer_url.to_string(),
            http_client: reqwest::Client::new(),
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

    /// Submit a midnight-privacy transaction to the verifier service
    ///
    /// This method submits a borsh-serialized transaction to the verifier service,
    /// which will verify the proof and then submit to the sequencer.
    /// Returns the transaction hash from the verifier response.
    pub async fn submit_to_verifier(
        &self,
        raw_tx: Vec<u8>,
        proof_ref: Option<&ProofRef>,
    ) -> Result<String> {
        use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
        use base64::Engine as _;

        let tx_body = if proof_ref.is_some() {
            let lightweight_tx = strip_midnight_proof_from_signed_tx(&raw_tx)
                .context("Failed to strip proof from signed midnight transaction")?;
            BASE64_STANDARD.encode(&lightweight_tx)
        } else {
            BASE64_STANDARD.encode(&raw_tx)
        };
        let proof_ref = proof_ref.cloned();
        // Trim trailing slash from verifier_url to avoid double slashes
        let base_url = self.verifier_url.trim_end_matches('/');
        let endpoint = format!("{}/midnight-privacy", base_url);

        tracing::info!("Submitting transaction to verifier service at {}", endpoint);
        tracing::debug!(
            "Transaction size: {} bytes, request body size: {} bytes",
            raw_tx.len(),
            tx_body.len()
        );

        let resp = self
            .http_client
            .post(&endpoint)
            .json(&serde_json::json!({
                "body": tx_body,
                "proof_ref": proof_ref,
            }))
            .send()
            .await
            .context("Failed to send transaction to verifier service")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(
                "Verifier service error - URL: {}, Status: {}, Body: {}",
                endpoint,
                status,
                body
            );
            anyhow::bail!(
                "Verifier service returned error status {}: {}",
                status,
                body
            );
        }

        let body = resp
            .text()
            .await
            .context("Failed to read verifier response")?;

        #[derive(serde::Deserialize)]
        struct VerifierResponse {
            success: bool,
            tx_hash: Option<String>,
            error: Option<String>,
        }

        let verifier_resp: VerifierResponse =
            serde_json::from_str(&body).context("Failed to parse verifier response")?;

        if !verifier_resp.success {
            anyhow::bail!(
                "Verifier service reported failure: {}",
                verifier_resp
                    .error
                    .unwrap_or_else(|| "Unknown error".to_string())
            );
        }

        let tx_hash = verifier_resp
            .tx_hash
            .ok_or_else(|| anyhow::anyhow!("Verifier response missing tx_hash"))?;

        tracing::info!("Transaction submitted via verifier, tx_hash: {}", tx_hash);

        Ok(tx_hash)
    }

    /// Get the RPC URL this provider is connected to
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Check if the rollup is healthy and responding
    ///
    /// Queries the `/healthcheck` endpoint to verify the rollup is available.
    #[cfg(any(test, feature = "test-utils"))]
    #[allow(dead_code)]
    pub async fn is_healthy(&self) -> bool {
        let health_url = format!("{}/healthcheck", self.rpc_url);

        match reqwest::get(&health_url).await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    /// Get transaction details from the indexer
    ///
    /// This queries the indexer API to retrieve full transaction details including
    /// all privacy-related fields, events, status, and payload.
    ///
    /// # Parameters
    /// * `tx_hash` - The transaction hash (with or without 0x prefix)
    ///
    /// # Returns
    /// Full transaction details if found, None if not found
    ///
    /// # Example
    /// ```rust,no_run
    /// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
    /// let tx = provider.get_transaction("0x1234...").await?;
    /// if let Some(tx) = tx {
    ///     println!("Transaction {}: {} at {}",
    ///         tx.tx_hash, tx.kind, tx.timestamp_ms);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_transaction(&self, tx_hash: &str) -> Result<Option<InvolvementItem>> {
        // Trim trailing slash from indexer_url to avoid double slashes
        let base_url = self.indexer_url.trim_end_matches('/');
        let url = format!("{}/transactions/{}", base_url, tx_hash);

        tracing::debug!("Fetching transaction details from indexer: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch transaction from indexer at {}", url))?;

        let status = response.status();

        // Handle 404 as "not found" rather than an error
        if status == reqwest::StatusCode::NOT_FOUND {
            tracing::debug!("Transaction {} not found in indexer", tx_hash);
            return Ok(None);
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Indexer returned error status {}: {}", status, body);
        }

        let tx: InvolvementItem = response
            .json()
            .await
            .context("Failed to parse transaction details from indexer")?;

        tracing::debug!(
            "Fetched transaction {} from indexer: kind={}, status={:?}",
            tx.tx_hash,
            tx.kind,
            tx.status
        );

        Ok(Some(tx))
    }

    /// Query a REST endpoint and deserialize the response
    ///
    /// Generic method to query any REST endpoint on the rollup node.
    pub async fn query_rest_endpoint<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
    ) -> Result<T> {
        self.client
            .query_rest_endpoint(endpoint)
            .await
            .with_context(|| format!("Failed to query REST endpoint: {}", endpoint))
    }

    /// Get transactions for a specific wallet address from the indexer
    ///
    /// This queries the indexer API to retrieve all transactions associated with
    /// the given wallet address. The indexer tracks both incoming and outgoing
    /// transactions, including deposits, withdrawals, and transfers.
    ///
    /// # Parameters
    /// * `address` - The wallet address to query transactions for
    /// * `limit` - Optional limit on the number of transactions to return (default: 50, max: 200)
    /// * `cursor` - Optional cursor for pagination
    /// * `tx_type` - Optional transaction type filter (e.g., "deposit", "withdraw")
    ///
    /// # Returns
    /// A list of transactions with their details
    ///
    /// # Example
    /// ```rust,no_run
    /// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
    /// let address = "0x1234...";
    /// let transactions = provider
    ///     .get_wallet_transactions(address, None, None, None)
    ///     .await?;
    /// println!("Found {} transactions", transactions.items.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_wallet_transactions(
        &self,
        address: &str,
        limit: Option<usize>,
        cursor: Option<&str>,
        tx_type: Option<&str>,
    ) -> Result<ListTransactionsResponse> {
        // Trim trailing slash from indexer_url to avoid double slashes
        let base_url = self.indexer_url.trim_end_matches('/');
        let mut url = format!("{}/transactions/wallet/{}/god", base_url, address);

        // Build query parameters
        let mut query_params = Vec::new();
        if let Some(limit) = limit {
            query_params.push(format!("limit={}", limit));
        }
        if let Some(cursor) = cursor {
            query_params.push(format!("cursor={}", cursor));
        }
        if let Some(tx_type) = tx_type {
            query_params.push(format!("type={}", tx_type));
        }

        if !query_params.is_empty() {
            url.push('?');
            url.push_str(&query_params.join("&"));
        }

        tracing::debug!("Fetching transactions from indexer: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch transactions from indexer at {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Indexer returned error status {}: {}", status, body);
        }

        let tx_list: ListTransactionsResponse = response
            .json()
            .await
            .context("Failed to parse indexer response")?;

        tracing::debug!(
            "Fetched {} transactions for address {}",
            tx_list.items.len(),
            address
        );

        Ok(tx_list)
    }

    /// Get all transactions from the indexer (global list, not filtered by address)
    ///
    /// This is used for privacy pool balance calculation, where we need to scan all
    /// transactions to find notes that belong to the user.
    ///
    /// # Parameters
    /// * `limit` - Optional limit on number of transactions per page (default: 100)
    /// * `cursor` - Optional pagination cursor (from `next` in the previous response)
    ///
    /// # Returns
    /// A list of all transactions from the indexer
    ///
    /// # Example
    /// ```rust,no_run
    /// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
    /// let transactions = provider.get_all_transactions(Some(100), None).await?;
    /// println!("Found {} transactions", transactions.items.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_transactions(
        &self,
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<ListTransactionsResponse> {
        // Trim trailing slash from indexer_url to avoid double slashes
        let base_url = self.indexer_url.trim_end_matches('/');
        let mut url = format!("{}/transactions/god", base_url);

        // Build query parameters
        let mut query_params = Vec::new();
        if let Some(limit) = limit {
            query_params.push(format!("limit={}", limit));
        }
        if let Some(cursor) = cursor {
            query_params.push(format!("cursor={}", cursor));
        }

        if !query_params.is_empty() {
            url.push('?');
            url.push_str(&query_params.join("&"));
        }

        tracing::debug!("Fetching all transactions from indexer: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch transactions from indexer at {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Indexer returned error status {}: {}", status, body);
        }

        let tx_list: ListTransactionsResponse = response
            .json()
            .await
            .context("Failed to parse transactions list from indexer")?;

        tracing::debug!(
            "Fetched {} transactions from indexer (cursor: {:?}, limit: {:?})",
            tx_list.items.len(),
            cursor,
            limit
        );

        Ok(tx_list)
    }
}
