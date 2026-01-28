//! Get transaction status
//!
//! This module provides functionality for retrieving the current status and details of a specific transaction.

use crate::provider::InvolvementItem;
use crate::provider::Provider;
use anyhow::{Context, Result};

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
    /// Privacy sender address (if available)
    pub privacy_sender: Option<String>,
    /// Privacy recipient address (if available)
    pub privacy_recipient: Option<String>,
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
    /// Decrypted notes for privacy transactions (when VFK is provided)
    pub decrypted_notes: Option<serde_json::Value>,
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
            privacy_sender: item.privacy_sender,
            privacy_recipient: item.privacy_recipient,
            amount: item.amount,
            anchor_root: item.anchor_root,
            nullifier: item.nullifier,
            view_fvks: item.view_fvks,
            view_attestations: item.view_attestations,
            events: item.events,
            encrypted_notes: item.encrypted_notes,
            decrypted_notes: item.decrypted_notes,
            payload: item.payload,
        }
    }
}

/// Fetch transaction status/details from the indexer by tx hash.
///
/// Returns an error if the transaction is not found.
pub async fn get_transaction_status(
    provider: &Provider,
    tx_hash: &str,
) -> Result<TransactionDetails> {
    let tx = provider
        .get_transaction(tx_hash)
        .await
        .with_context(|| format!("Failed to fetch transaction {} from indexer", tx_hash))?
        .ok_or_else(|| anyhow::anyhow!("Transaction not found: {}", tx_hash))?;
    Ok(TransactionDetails::from(tx))
}
