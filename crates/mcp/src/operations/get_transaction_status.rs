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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_get_transaction_status() {
        use crate::test_utils::is_rollup_available;

        if !is_rollup_available().await {
            eprintln!("⚠️  Skipping test: Rollup is not available at ROLLUP_RPC_URL");
            eprintln!("   Start the rollup or set ROLLUP_RPC_URL to run this test");
            return;
        }

        let rpc_url = std::env::var("ROLLUP_RPC_URL")
            .unwrap_or_else(|_| "http://localhost:12346".to_string());
        let verifier_url =
            std::env::var("VERIFIER_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
        let indexer_url =
            std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

        tracing::info!("Connecting to rollup at: {}", rpc_url);
        let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url)
            .await
            .expect("Failed to connect to rollup");

        let tx_hash = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        tracing::info!("Testing get_transaction_status with tx_hash: {}", tx_hash);

        // Wait a bit for the transaction to be indexed
        tracing::info!("Waiting 2 seconds for transaction to be indexed...");
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        tracing::info!("Getting transaction details for: {}", tx_hash);
        let details = get_transaction_status(&provider, &tx_hash)
            .await
            .expect("Failed to get transaction details");

        tracing::info!("✅ Transaction status: {}", details.status);
        tracing::info!("   Transaction kind: {:?}", details.kind);
        tracing::info!("   Timestamp: {:?}", details.timestamp_ms);

        assert_eq!(
            details.tx_hash.to_lowercase(),
            tx_hash.to_lowercase(),
            "Transaction hash should match the submitted hash"
        );
        assert!(
            !details.status.is_empty(),
            "Transaction status should not be empty"
        );

        tracing::info!("✅ Test completed successfully!");
    }
}
