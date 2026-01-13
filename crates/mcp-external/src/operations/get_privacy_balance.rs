//! Get Privacy Pool Balance Operation

use anyhow::{Context, Result};
use midnight_privacy::FullViewingKey;
use serde::{Deserialize, Serialize};

use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;

const DOMAIN: [u8; 32] = [1u8; 32];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnspentNote {
    pub value: u128,
    pub rho: String,
    pub tx_hash: String,
    pub timestamp_ms: i64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyBalanceResult {
    pub balance: u128,
    pub unspent_notes: Vec<UnspentNote>,
    pub deposit_count: usize,
    pub transfer_count: usize,
    pub withdraw_count: usize,
    pub total_transactions_scanned: usize,
}

/// Get the privacy pool balance for the user.
///
/// Uses the indexer's `/wallets/:address/balance` endpoint which efficiently
/// computes the balance by decrypting notes and tracking spent nullifiers.
pub async fn get_privacy_balance(
    provider: &Provider,
    privacy_key: &PrivacyKey,
    viewing_key: &FullViewingKey,
) -> Result<PrivacyBalanceResult> {
    let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

    tracing::info!(
        "Fetching privacy pool balance for address {}",
        privacy_address
    );

    // Get spend_sk from privacy key
    let spend_sk = privacy_key
        .spend_sk()
        .ok_or_else(|| anyhow::anyhow!("Privacy key must have spend_sk to calculate balance"))?;
    let spend_sk_hex = hex::encode(spend_sk);

    // Get VFK as hex
    let vfk_hex = hex::encode(viewing_key.0);

    // Call the indexer's balance endpoint
    let balance_response = provider
        .get_wallet_balance(&privacy_address, &spend_sk_hex, Some(&vfk_hex))
        .await
        .context("Failed to fetch balance from indexer")?;

    // Parse balance
    let balance = balance_response
        .balance
        .parse::<u128>()
        .context("Failed to parse balance from indexer response")?;

    // Convert unspent notes to our format
    let unspent_notes: Vec<UnspentNote> = balance_response
        .unspent_notes
        .into_iter()
        .map(|note| {
            let value = note.value.parse::<u128>().unwrap_or(0);
            UnspentNote {
                value,
                rho: note.rho,
                tx_hash: note.tx_hash,
                timestamp_ms: note.timestamp_ms,
                kind: note.kind,
            }
        })
        .collect();

    tracing::info!(
        "Privacy pool balance: {}, unspent notes: {}",
        balance,
        unspent_notes.len()
    );

    // Calculate transaction counts from notes
    let deposit_count = unspent_notes.iter().filter(|n| n.kind == "deposit").count();
    let transfer_count = unspent_notes.iter().filter(|n| n.kind == "transfer").count();
    let withdraw_count = 0; // Withdrawals don't create unspent notes for the user

    Ok(PrivacyBalanceResult {
        balance,
        unspent_notes,
        deposit_count,
        transfer_count,
        withdraw_count,
        total_transactions_scanned: deposit_count + transfer_count,
    })
}
