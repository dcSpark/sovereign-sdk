//! Get all transactions for the privacy pool
//!
//! This module provides functionality for retrieving a list of all transactions
//! associated with the privacy pool address from the indexer API.

use crate::privacy_key::PrivacyKey;
use crate::provider::{InvolvementItem, Provider};
use crate::wallet::WalletContext;
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Transaction information from indexer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
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
    /// Transaction amount (if available)
    pub amount: Option<String>,
    /// Anchor root for privacy transactions
    pub anchor_root: Option<String>,
    /// Nullifier for privacy transactions
    pub nullifier: Option<String>,
    /// View Full Viewing Keys (FVKs) for note decryption
    pub view_fvks: Option<serde_json::Value>,
    /// View attestations for privacy proofs
    pub view_attestations: Option<serde_json::Value>,
    /// Transaction events from the rollup
    pub events: Option<serde_json::Value>,
    /// Transaction status (e.g., "Success", "Failed")
    pub status: Option<String>,
    /// Encrypted notes for privacy transactions
    pub encrypted_notes: Option<serde_json::Value>,
    /// Full transaction payload
    pub payload: Option<serde_json::Value>,
}

impl From<InvolvementItem> for Transaction {
    fn from(item: InvolvementItem) -> Self {
        Self {
            tx_hash: item.tx_hash,
            timestamp_ms: item.timestamp_ms,
            kind: item.kind,
            sender: item.sender,
            recipient: item.recipient,
            amount: item.amount,
            anchor_root: item.anchor_root,
            nullifier: item.nullifier,
            view_fvks: item.view_fvks,
            view_attestations: item.view_attestations,
            events: item.events,
            status: item.status,
            encrypted_notes: item.encrypted_notes,
            payload: item.payload,
        }
    }
}

/// Get all transactions for the privacy pool
///
/// Retrieves a list of all transactions associated with the privacy pool address from the indexer API.
/// As per CLAUDE.md, this MCP operates inside the privacy pool, so only privacy pool transactions
/// (deposits, transfers, and withdrawals) are returned.
///
/// The indexer tracks all privacy pool activity including deposits into the pool,
/// transfers within the pool, and withdrawals from the pool.
///
/// # Parameters
/// * `provider` - The RPC provider with indexer access
/// * `wallet` - The wallet context containing the address
/// * `privacy_key` - The privacy key for deriving the privacy address
///
/// # Returns
/// A vector of transactions with full details from the indexer
///
/// # Example
/// ```rust,no_run
/// # async fn example<Tx, S>(
/// #     provider: &mcp::provider::Provider,
/// #     wallet: &mcp::wallet::WalletContext<Tx, S>,
/// #     privacy_key: &mcp::privacy_key::PrivacyKey,
/// # ) -> anyhow::Result<()>
/// # where
/// #     Tx: sov_modules_api::DispatchCall,
/// #     Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
/// #     S: sov_modules_api::Spec,
/// # {
/// use mcp::operations::get_transactions;
///
/// let transactions = get_transactions(provider, wallet, privacy_key).await?;
/// println!("Found {} transactions", transactions.len());
/// for tx in transactions {
///     println!(
///         "Transaction {}: {} at {} (status: {:?})",
///         tx.tx_hash, tx.kind, tx.timestamp_ms, tx.status
///     );
/// }
/// # Ok(())
/// # }
/// ```
pub async fn get_transactions<Tx, S>(
    provider: &Provider,
    _wallet: &WalletContext<Tx, S>,
    privacy_key: &PrivacyKey,
) -> Result<Vec<Transaction>>
where
    Tx: sov_modules_api::DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: sov_modules_api::Spec,
{
    // Get the privacy pool address (as per CLAUDE.md: this MCP operates inside the privacy pool)
    let privacy_address = privacy_key.privacy_address().to_string();

    tracing::debug!(
        "Fetching transactions for privacy pool address: {}",
        privacy_address
    );

    // Query the indexer for transactions from the privacy pool address only
    let privacy_response = provider
        .get_wallet_transactions(&privacy_address, None, None, None)
        .await?;

    tracing::info!(
        "Retrieved {} transactions for privacy pool address {}",
        privacy_response.items.len(),
        privacy_address
    );

    // Convert to Transaction type
    let mut all_transactions: Vec<Transaction> = privacy_response
        .items
        .into_iter()
        .map(Transaction::from)
        .collect();

    // Sort by timestamp (newest first)
    all_transactions.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));

    tracing::info!(
        "Total transactions for privacy pool: {}",
        all_transactions.len()
    );

    Ok(all_transactions)
}
