//! RPC Provider for interacting with the Sovereign rollup
//!
//! This module handles all RPC communication with the rollup node, including:
//! - Chain state queries (nonce)
//! - Transaction submission
//! - Fee estimation
//! - Block queries
//! - Schema queries (chain_id, chain_name)

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use sov_api_spec::types;
use sov_bank::TokenId;
use sov_modules_api::{CryptoSpec, Spec};
use sov_node_client::NodeClient;

/// Chain data from the rollup schema
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChainData {
    /// The chain ID
    pub chain_id: u64,
    /// The chain name
    pub chain_name: String,
}

/// Result from the verifier submission endpoint.
#[derive(Debug, Clone)]
pub struct VerifierSubmitResult {
    pub tx_hash: String,
    pub created_at: i64,
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

/// Unspent note from the indexer's balance endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UnspentNote {
    /// Note value
    pub value: String,
    /// Note rho (hex encoded)
    pub rho: String,
    /// Optional sender ID (hex encoded, for transfers)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// Transaction hash where the note was created
    pub tx_hash: String,
    /// Timestamp in milliseconds
    pub timestamp_ms: i64,
    /// Transaction kind ("deposit", "transfer", etc.)
    pub kind: String,
}

/// Response from the indexer's balance endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BalanceResponse {
    /// Total balance as string
    pub balance: String,
    /// List of unspent notes
    pub unspent_notes: Vec<UnspentNote>,
}

/// Response from the indexer's prefunded wallets import endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PrefundedWalletImportSummary {
    pub processed: usize,
    pub inserted: usize,
    pub ignored: usize,
}

/// Claimed prefunded wallet metadata returned by the indexer
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ClaimedPrefundedWallet {
    pub wallet_address: String,
    pub privacy_address: String,
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

const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 15;
const DEFAULT_HTTP_CONNECT_TIMEOUT_SECS: u64 = 5;
const DEFAULT_VERIFIER_SUBMIT_RETRIES: u32 = 3;
const DEFAULT_VERIFIER_SUBMIT_RETRY_DELAY_MS: u64 = 1000;

fn http_timeout_secs() -> u64 {
    std::env::var("MCP_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_SECS)
}

fn http_connect_timeout_secs() -> u64 {
    std::env::var("MCP_HTTP_CONNECT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_HTTP_CONNECT_TIMEOUT_SECS)
}

fn verifier_submit_retries() -> u32 {
    std::env::var("MCP_VERIFIER_SUBMIT_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_VERIFIER_SUBMIT_RETRIES)
}

fn verifier_submit_retry_delay_ms() -> u64 {
    std::env::var("MCP_VERIFIER_SUBMIT_RETRY_DELAY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_VERIFIER_SUBMIT_RETRY_DELAY_MS)
}

impl Provider {
    /// Create a new provider connected to the given RPC URL, verifier service, and indexer
    pub async fn new(rpc_url: &str, verifier_url: &str, indexer_url: &str) -> Result<Self> {
        let client = NodeClient::new(rpc_url)
            .await
            .with_context(|| format!("Failed to connect to rollup node at {}", rpc_url))?;

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(http_timeout_secs()))
            .connect_timeout(Duration::from_secs(http_connect_timeout_secs()))
            .build()
            .context("Failed to create HTTP client for provider")?;

        Ok(Self {
            client: Arc::new(client),
            rpc_url: rpc_url.to_string(),
            verifier_url: verifier_url.to_string(),
            indexer_url: indexer_url.to_string(),
            http_client,
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
    /// # async fn example(provider: &mcp_external::provider::Provider) -> anyhow::Result<()> {
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

    /// Get a bank balance for the given address and token id.
    pub async fn get_balance<S: Spec>(
        &self,
        account_address: &S::Address,
        token_id: &TokenId,
    ) -> Result<sov_bank::Amount> {
        self.client
            .get_balance::<S>(account_address, token_id, None)
            .await
            .context("Failed to get balance from rollup")
    }

    pub async fn get_gas_token_id(&self) -> Result<TokenId> {
        #[derive(Deserialize)]
        struct TokenIdResponse {
            token_id: TokenId,
        }

        let response: TokenIdResponse = self
            .query_rest_endpoint("/modules/bank/tokens/gas_token")
            .await
            .context("Failed to fetch gas token id from rollup")?;

        Ok(response.token_id)
    }

    /// Get transaction details from the sequencer by tx hash.
    pub async fn get_sequencer_tx(&self, tx_hash: &str) -> Result<Option<types::ApiAcceptedTx>> {
        let parsed: types::TxHash = tx_hash
            .parse()
            .with_context(|| format!("Failed to parse tx hash '{}'", tx_hash))?;

        match self.client.client.sequencer_get_tx(&parsed).await {
            Ok(tx_info) => Ok(Some(tx_info.into_inner())),
            Err(sov_api_spec::Error::ErrorResponse(response)) => {
                if response.status().as_u16() == 404 {
                    Ok(None)
                } else {
                    let status = response.status();
                    let error = response.into_inner();
                    anyhow::bail!(
                        "Sequencer returned error status {}: {}",
                        status,
                        error.message
                    );
                }
            }
            Err(err) => Err(anyhow::anyhow!(
                "Failed to fetch tx details from sequencer: {err}"
            )),
        }
    }

    /// Submit a raw transaction to the rollup sequencer
    ///
    /// This method accepts a borsh-serialized `Transaction` and submits it to the sequencer.
    /// Returns the transaction hash from the rollup.
    #[allow(dead_code)]
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
    /// Returns the transaction hash (and optional createdAt) from the verifier response.
    pub async fn submit_to_verifier(&self, raw_tx: Vec<u8>) -> Result<VerifierSubmitResult> {
        use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
        use base64::Engine as _;

        let tx_b64 = BASE64_STANDARD.encode(&raw_tx);
        // Trim trailing slash from verifier_url to avoid double slashes
        let base_url = self.verifier_url.trim_end_matches('/');
        let endpoint = format!("{}/midnight-privacy", base_url);

        tracing::debug!("Submitting transaction to verifier service at {}", endpoint);
        tracing::debug!(
            "Transaction size: {} bytes, base64 size: {} bytes",
            raw_tx.len(),
            tx_b64.len()
        );

        let max_retries = verifier_submit_retries();
        let retry_delay = Duration::from_millis(verifier_submit_retry_delay_ms());
        let mut last_err: Option<anyhow::Error> = None;

        let resp = 'submit: {
            for attempt in 0..=max_retries {
                match self
                    .http_client
                    .post(&endpoint)
                    .json(&serde_json::json!({ "body": tx_b64 }))
                    .send()
                    .await
                {
                    Ok(r) => break 'submit r,
                    Err(e) => {
                        if attempt < max_retries {
                            let delay = retry_delay * (attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts = max_retries + 1,
                                retry_delay_ms = delay.as_millis(),
                                error = %e,
                                "Verifier submission failed (transient); retrying"
                            );
                            tokio::time::sleep(delay).await;
                            last_err = Some(e.into());
                        } else {
                            last_err = Some(e.into());
                        }
                    }
                }
            }
            return Err(last_err
                .unwrap_or_else(|| anyhow::anyhow!("Verifier submission failed"))
                .context("Failed to send transaction to verifier service after retries"));
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let parsed: Option<serde_json::Value> = serde_json::from_str(&body).ok();
            let reason = parsed
                .as_ref()
                .and_then(|v| v.get("details"))
                .and_then(|d| d.get("error"))
                .and_then(|e| e.as_str())
                .or_else(|| {
                    parsed
                        .as_ref()
                        .and_then(|v| v.get("message"))
                        .and_then(|m| m.as_str())
                })
                .map(str::to_string);
            tracing::error!(
                "Verifier service error - URL: {}, Status: {}, Body: {}",
                endpoint,
                status,
                body
            );
            match reason {
                Some(reason) => anyhow::bail!(
                    "Verifier service returned error status {}: {}",
                    status,
                    reason
                ),
                None => anyhow::bail!(
                    "Verifier service returned error status {}: {}",
                    status,
                    body
                ),
            }
        }

        let body = resp
            .text()
            .await
            .context("Failed to read verifier response")?;

        #[derive(Deserialize)]
        struct VerifierMetrics {
            #[serde(rename = "createdAt")]
            created_at: String,
        }

        #[derive(Deserialize)]
        struct VerifierResponse {
            success: bool,
            #[serde(rename = "tx_hash", alias = "id")]
            tx_hash: Option<String>,
            metrics: VerifierMetrics,
            error: Option<String>,
        }

        let verifier_resp: VerifierResponse =
            serde_json::from_str(&body).context("Failed to parse verifier response")?;

        if !verifier_resp.success {
            let error = verifier_resp.error.as_deref().unwrap_or("Unknown error");
            anyhow::bail!("Verifier service reported failure: {}", error);
        }

        let tx_hash = verifier_resp
            .tx_hash
            .ok_or_else(|| anyhow::anyhow!("Verifier response missing tx_hash"))?;
        let created_at = parse_rfc3339_to_millis(&verifier_resp.metrics.created_at)
            .ok_or_else(|| anyhow::anyhow!("Verifier response missing metrics.createdAt"))?;

        tracing::debug!("Transaction submitted via verifier, tx_hash: {}", tx_hash);

        Ok(VerifierSubmitResult {
            tx_hash: tx_hash.to_string(),
            created_at,
        })
    }

    /// Get the RPC URL this provider is connected to
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Get the indexer URL this provider is configured with
    pub fn indexer_url(&self) -> &str {
        &self.indexer_url
    }

    /// Check if the rollup is healthy and responding
    ///
    /// Queries the `/healthcheck` endpoint to verify the rollup is available.
    #[allow(dead_code)] // Used by integration tests to gate network-dependent flows
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
    /// # async fn example(provider: &mcp_external::provider::Provider) -> anyhow::Result<()> {
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
    /// # async fn example(provider: &mcp_external::provider::Provider) -> anyhow::Result<()> {
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
            anyhow::bail!(
                "Indexer API error at {}: HTTP {} - {}",
                url,
                status,
                if body.is_empty() {
                    "No error details provided"
                } else {
                    &body
                }
            );
        }

        let tx_list: ListTransactionsResponse = response.json().await.with_context(|| {
            format!(
                "Failed to parse wallet transactions JSON from indexer at {}",
                url
            )
        })?;

        tracing::debug!(
            "Fetched {} transactions for address {}",
            tx_list.items.len(),
            address
        );

        Ok(tx_list)
    }

    /// Get wallet balance from the indexer
    ///
    /// Uses the indexer's `/wallets/:address/balance` endpoint which efficiently
    /// computes the balance by decrypting notes and tracking spent nullifiers.
    ///
    /// # Parameters
    /// * `address` - Privacy address (bech32m format)
    /// * `spend_sk_hex` - Optional spending secret key as hex string (with or without 0x prefix)
    /// * `nf_key_hex` - Optional nullifier key as hex string (with or without 0x prefix)
    /// * `vfk_hex` - Optional viewing key for decrypting encrypted notes
    ///
    /// # Returns
    /// Balance response with total balance and list of unspent notes
    pub async fn get_wallet_balance(
        &self,
        address: &str,
        spend_sk_hex: Option<&str>,
        nf_key_hex: Option<&str>,
        vfk_hex: Option<&str>,
    ) -> Result<BalanceResponse> {
        let base_url = self.indexer_url.trim_end_matches('/');
        let url = format!("{}/wallets/{}/balance", base_url, address);

        // Build request body
        let mut body = serde_json::Map::new();
        if let Some(spend_sk) = spend_sk_hex {
            body.insert(
                "spend_sk".to_string(),
                serde_json::Value::String(spend_sk.to_string()),
            );
        }
        if let Some(nf_key) = nf_key_hex {
            body.insert(
                "nf_key".to_string(),
                serde_json::Value::String(nf_key.to_string()),
            );
        }
        if body.is_empty() {
            anyhow::bail!("spend_sk or nf_key is required to fetch wallet balance");
        }
        if let Some(vfk) = vfk_hex {
            body.insert(
                "vfk".to_string(),
                serde_json::Value::String(vfk.to_string()),
            );
        }

        tracing::debug!("Fetching balance from indexer: {}", url);

        let response = self
            .http_client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to fetch balance from indexer at {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Indexer balance API error at {}: HTTP {} - {}",
                url,
                status,
                if body.is_empty() {
                    "No error details provided"
                } else {
                    &body
                }
            );
        }

        let balance_response: BalanceResponse = response
            .json()
            .await
            .with_context(|| format!("Failed to parse balance JSON from indexer at {}", url))?;

        tracing::debug!(
            "Fetched balance for address {}: {}",
            address,
            balance_response.balance
        );

        Ok(balance_response)
    }

    pub async fn import_prefunded_wallets(
        &self,
        wallets: &[crate::prefunded_wallets::PrefundedWalletImportItem],
    ) -> Result<PrefundedWalletImportSummary> {
        #[derive(serde::Serialize)]
        struct ImportRequest<'a> {
            wallets: &'a [crate::prefunded_wallets::PrefundedWalletImportItem],
        }

        let base_url = self.indexer_url.trim_end_matches('/');
        let url = format!("{}/prefunded-wallets/import", base_url);

        let response = self
            .http_client
            .post(&url)
            .json(&ImportRequest { wallets })
            .send()
            .await
            .with_context(|| format!("Failed to import prefunded wallets at {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Indexer prefunded-wallets import error at {}: HTTP {} - {}",
                url,
                status,
                if body.is_empty() {
                    "No error details provided"
                } else {
                    &body
                }
            );
        }

        let summary: PrefundedWalletImportSummary = response.json().await.with_context(|| {
            format!(
                "Failed to parse prefunded-wallets import response JSON from indexer at {}",
                url
            )
        })?;

        Ok(summary)
    }

    pub async fn claim_prefunded_wallet(
        &self,
        claimed_by: Option<&str>,
    ) -> Result<Option<ClaimedPrefundedWallet>> {
        #[derive(serde::Serialize)]
        struct ClaimRequest<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            claimed_by: Option<&'a str>,
        }

        let base_url = self.indexer_url.trim_end_matches('/');
        let url = format!("{}/prefunded-wallets/claim", base_url);

        let response = self
            .http_client
            .post(&url)
            .json(&ClaimRequest { claimed_by })
            .send()
            .await
            .with_context(|| format!("Failed to claim prefunded wallet at {}", url))?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Indexer prefunded-wallets claim error at {}: HTTP {} - {}",
                url,
                status,
                if body.is_empty() {
                    "No error details provided"
                } else {
                    &body
                }
            );
        }

        let claimed: ClaimedPrefundedWallet = response.json().await.with_context(|| {
            format!(
                "Failed to parse prefunded-wallets claim response JSON from indexer at {}",
                url
            )
        })?;
        Ok(Some(claimed))
    }
}

fn parse_rfc3339_to_millis(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// FVK entry from the indexer's FVK registry
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FvkEntry {
    /// FVK commitment (unique identifier)
    pub fvk_commitment: String,
    /// Full Viewing Key (hex encoded)
    pub fvk: String,
    /// Associated shielded address (optional, bech32m privpool1...)
    #[serde(default)]
    pub shielded_address: Option<String>,
    /// Associated public wallet address (optional, sov1...)
    #[serde(default)]
    pub wallet_address: Option<String>,
}

/// Response from the indexer's FVK list endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FvkListResponse {
    pub count: usize,
    pub fvks: Vec<FvkEntry>,
}

impl Provider {
    /// Get all registered FVKs from the indexer
    ///
    /// This queries the indexer's `/fvks` endpoint to retrieve all registered
    /// Full Viewing Keys and their associated shielded addresses.
    pub async fn get_fvk_registry(&self) -> Result<FvkListResponse> {
        let base_url = self.indexer_url.trim_end_matches('/');
        let url = format!("{}/fvks", base_url);

        tracing::debug!("Fetching FVK registry from indexer: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch FVK registry from indexer at {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Indexer FVK registry API error at {}: HTTP {} - {}",
                url,
                status,
                if body.is_empty() {
                    "No error details provided"
                } else {
                    &body
                }
            );
        }

        let fvk_list: FvkListResponse = response.json().await.with_context(|| {
            format!("Failed to parse FVK registry JSON from indexer at {}", url)
        })?;

        tracing::debug!("Fetched {} FVKs from indexer", fvk_list.count);

        Ok(fvk_list)
    }
}
