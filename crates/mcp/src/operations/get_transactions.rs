//! Get all transactions for the wallet
//!
//! This module provides functionality for retrieving a list of all transactions
//! associated with the wallet from the indexer API.

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
    /// Direction of involvement (e.g., "in", "out")
    pub direction: String,
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
}

impl From<InvolvementItem> for Transaction {
    fn from(item: InvolvementItem) -> Self {
        Self {
            tx_hash: item.tx_hash,
            timestamp_ms: item.timestamp_ms,
            kind: item.kind,
            direction: item.direction,
            sender: item.sender,
            recipient: item.recipient,
            amount: item.amount,
            anchor_root: item.anchor_root,
            nullifier: item.nullifier,
        }
    }
}

/// Get all transactions for the wallet
///
/// Retrieves a list of all transactions associated with the wallet from the indexer API.
/// The indexer tracks all wallet activity including deposits, withdrawals, and transfers.
///
/// # Parameters
/// * `provider` - The RPC provider with indexer access
/// * `wallet` - The wallet context containing the address
///
/// # Returns
/// A vector of transactions with full details from the indexer
///
/// # Example
/// ```rust,no_run
/// # async fn example<Tx, S>(
/// #     provider: &mcp::provider::Provider,
/// #     wallet: &mcp::wallet::WalletContext<Tx, S>
/// # ) -> anyhow::Result<()>
/// # where
/// #     Tx: sov_modules_api::DispatchCall,
/// #     Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
/// #     S: sov_modules_api::Spec,
/// # {
/// use mcp::operations::get_transactions;
///
/// let transactions = get_transactions(provider, wallet).await?;
/// println!("Found {} transactions", transactions.len());
/// for tx in transactions {
///     println!("Transaction {}: {} {} at {}",
///         tx.tx_hash, tx.kind, tx.direction, tx.timestamp_ms);
/// }
/// # Ok(())
/// # }
/// ```
pub async fn get_transactions<Tx, S>(
    provider: &Provider,
    wallet: &WalletContext<Tx, S>,
) -> Result<Vec<Transaction>>
where
    Tx: sov_modules_api::DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: sov_modules_api::Spec,
{
    // Get the wallet address and convert to string
    let address = wallet.get_address();
    let address_str = address.to_string();

    tracing::debug!("Fetching transactions for wallet address: {}", address_str);

    // Query the indexer for transactions
    // Using default limit (50) and no pagination for now
    let response = provider
        .get_wallet_transactions(&address_str, None, None, None)
        .await?;

    tracing::info!(
        "Retrieved {} transactions for wallet {}",
        response.items.len(),
        address
    );

    // Convert InvolvementItems to Transaction structs
    let transactions: Vec<Transaction> = response
        .items
        .into_iter()
        .map(Transaction::from)
        .collect();

    Ok(transactions)
}
