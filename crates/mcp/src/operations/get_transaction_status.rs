//! Get transaction status
//!
//! This module provides functionality for retrieving the current status and details of a specific transaction.

use crate::provider::{InvolvementItem, Provider};
use anyhow::{anyhow, Context, Result};

/// Transaction status and details from the indexer
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransactionDetails {
    /// Transaction hash
    pub tx_hash: String,
    /// Transaction status (e.g., "Success", "Failed", or "pending" if not yet indexed)
    pub status: String,
    /// Timestamp in milliseconds (if available)
    pub timestamp_ms: Option<i64>,
    /// Transaction kind (e.g., "deposit", "withdraw", "transfer")
    pub kind: Option<String>,
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
    /// Encrypted notes for privacy transactions
    pub encrypted_notes: Option<serde_json::Value>,
    /// Full transaction payload
    pub payload: Option<serde_json::Value>,
}

impl From<InvolvementItem> for TransactionDetails {
    fn from(item: InvolvementItem) -> Self {
        Self {
            tx_hash: item.tx_hash,
            status: item.status.unwrap_or_else(|| "Unknown".to_string()),
            timestamp_ms: Some(item.timestamp_ms),
            kind: Some(item.kind),
            sender: item.sender,
            recipient: item.recipient,
            amount: item.amount,
            anchor_root: item.anchor_root,
            nullifier: item.nullifier,
            view_fvks: item.view_fvks,
            view_attestations: item.view_attestations,
            events: item.events,
            encrypted_notes: item.encrypted_notes,
            payload: item.payload,
        }
    }
}

/// Get the status and details of a transaction by its ID
///
/// Retrieves full transaction details from the indexer including status, kind,
/// amounts, and all privacy-related fields.
///
/// # Parameters
/// * `provider` - The RPC provider with indexer access
/// * `tx_hash` - The transaction hash ID (with or without 0x prefix)
///
/// # Returns
/// Transaction details including status and all available metadata
///
/// # Example
/// ```rust,no_run
/// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
/// use mcp::operations::get_transaction_status;
///
/// let details = get_transaction_status(provider, "0x1234...").await?;
/// println!("Transaction {} status: {}", details.tx_hash, details.status);
/// # Ok(())
/// # }
/// ```
pub async fn get_transaction_status(
    provider: &Provider,
    tx_hash: &str,
) -> Result<TransactionDetails> {
    tracing::info!("Getting transaction details from indexer: {}", tx_hash);

    let tx_option = provider
        .get_transaction(tx_hash)
        .await
        .with_context(|| format!("Failed to get transaction details for {}", tx_hash))?;

    let tx = tx_option.ok_or_else(|| {
        anyhow!(
            "Transaction {} not found in indexer (may not be indexed yet)",
            tx_hash
        )
    })?;

    let details = TransactionDetails::from(tx);

    tracing::info!(
        "Transaction {} status: {}, kind: {:?}",
        details.tx_hash,
        details.status,
        details.kind
    );

    Ok(details)
}
