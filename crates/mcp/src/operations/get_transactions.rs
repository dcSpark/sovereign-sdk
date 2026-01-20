//! Get all transactions for the wallet
//!
//! This module provides functionality for retrieving a list of all transactions
//! associated with the wallet from the indexer API.

use crate::privacy_key::PrivacyKey;
use crate::provider::{InvolvementItem, Provider};
use crate::wallet::WalletContext;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const DOMAIN: [u8; 32] = [1u8; 32];

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
    /// Transaction status (e.g., "Success", "Failed")
    pub status: Option<String>,
    /// Encrypted notes for privacy transactions
    pub encrypted_notes: Option<serde_json::Value>,
    /// Decrypted notes for privacy transactions (when VFK is provided)
    pub decrypted_notes: Option<serde_json::Value>,
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
            privacy_sender: item.privacy_sender,
            privacy_recipient: item.privacy_recipient,
            amount: item.amount,
            anchor_root: item.anchor_root,
            nullifier: item.nullifier,
            view_fvks: item.view_fvks,
            view_attestations: item.view_attestations,
            events: item.events,
            status: item.status,
            encrypted_notes: item.encrypted_notes,
            decrypted_notes: item.decrypted_notes,
            payload: item.payload,
        }
    }
}

/// Get all transactions for the wallet
///
/// Retrieves a list of all transactions associated with the wallet from the indexer API.
/// This queries both:
/// - The normal wallet address (for deposits from L2)
/// - The privacy address (for transfers from/to the shielded pool)
///
/// The indexer tracks all wallet activity including deposits, withdrawals, and transfers.
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
    wallet: &WalletContext<Tx, S>,
    privacy_key: &PrivacyKey,
) -> Result<Vec<Transaction>>
where
    Tx: sov_modules_api::DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: sov_modules_api::Spec,
{
    // Get the wallet address and privacy address
    let address = wallet.get_address();
    let address_str = address.to_string();
    let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

    tracing::debug!(
        "Fetching transactions for wallet address: {} and privacy address: {}",
        address_str,
        privacy_address
    );

    // Query the indexer for transactions from the normal wallet address (deposits)
    let normal_response = provider
        .get_wallet_transactions(&address_str, None, None, None, None)
        .await?;

    tracing::info!(
        "Retrieved {} transactions for normal wallet {}",
        normal_response.items.len(),
        address_str
    );

    // Query the indexer for transactions from the privacy address (transfers)
    let privacy_response = provider
        .get_wallet_transactions(&privacy_address, None, None, None, None)
        .await?;

    tracing::info!(
        "Retrieved {} transactions for privacy address {}",
        privacy_response.items.len(),
        privacy_address
    );

    // Merge and deduplicate transactions by tx_hash
    let mut seen_hashes: HashSet<String> = HashSet::new();
    let mut all_transactions: Vec<Transaction> = Vec::new();

    // Add transactions from normal wallet
    for item in normal_response.items {
        if seen_hashes.insert(item.tx_hash.clone()) {
            all_transactions.push(Transaction::from(item));
        }
    }

    // Add transactions from privacy address (skip duplicates)
    for item in privacy_response.items {
        if seen_hashes.insert(item.tx_hash.clone()) {
            all_transactions.push(Transaction::from(item));
        }
    }

    // Sort by timestamp (newest first)
    all_transactions.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));

    tracing::info!(
        "Total unique transactions after merging: {}",
        all_transactions.len()
    );

    Ok(all_transactions)
}
