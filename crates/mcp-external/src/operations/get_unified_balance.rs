//! Unified Balance Operation
//!
//! This module provides a unified view of both transparent L2 balance and privacy pool balance.

use anyhow::{Context, Result};
use midnight_privacy::FullViewingKey;
use serde::{Deserialize, Serialize};
use sov_modules_api::{DispatchCall, Spec};

use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::wallet::WalletContext;

use super::get_default_token_balance;
use super::get_privacy_balance::{get_privacy_balance, UnspentNote};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedBalanceResult {
    /// Wallet address
    pub address: String,
    /// Transparent L2 balance (from Bank module)
    pub transparent_balance: String,
    /// Privacy pool balance (shielded)
    pub privacy_balance: String,
    /// Unspent notes in privacy pool
    pub unspent_notes: Vec<UnspentNote>,
    /// Number of deposits received
    pub deposit_count: usize,
    /// Number of transfers (incoming + outgoing)
    pub transfer_count: usize,
    /// Number of withdrawals
    pub withdraw_count: usize,
    /// Total transactions scanned for privacy balance
    pub total_transactions_scanned: usize,
}

/// Get unified balance (both transparent L2 and privacy pool)
///
/// # Parameters
/// * `provider` - The RPC provider for querying chain state
/// * `wallet` - The wallet context for address information
/// * `token_id` - The token ID as a bech32 string
/// * `privacy_key` - The privacy key for recipient derivation
/// * `viewing_key` - The viewing key for decrypting notes
pub async fn get_unified_balance<Tx, S>(
    provider: &Provider,
    wallet: &WalletContext<Tx, S>,
    token_id: &str,
    privacy_key: &PrivacyKey,
    viewing_key: &FullViewingKey,
) -> Result<UnifiedBalanceResult>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    // Get transparent L2 balance
    let (address, transparent_amount) = get_default_token_balance(provider, wallet, token_id)
        .await
        .context("Failed to fetch transparent balance from rollup")?;

    // Get privacy pool balance
    let privacy_result = get_privacy_balance(provider, privacy_key, viewing_key).await
        .context("Failed to calculate privacy pool balance. Ensure the indexer is running and accessible")?;

    // Convert Amount to string for transparent balance
    let transparent_balance_str = transparent_amount.to_string();
    let transparent_balance_value: u128 = transparent_balance_str.parse().unwrap_or_else(|_| {
        tracing::warn!("Failed to parse transparent balance, using 0");
        0
    });
    let privacy_balance_value = privacy_result.balance;

    Ok(UnifiedBalanceResult {
        address,
        transparent_balance: transparent_balance_value.to_string(),
        privacy_balance: privacy_balance_value.to_string(),
        unspent_notes: privacy_result.unspent_notes,
        deposit_count: privacy_result.deposit_count,
        transfer_count: privacy_result.transfer_count,
        withdraw_count: privacy_result.withdraw_count,
        total_transactions_scanned: privacy_result.total_transactions_scanned,
    })
}
