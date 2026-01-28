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
    /// Sender identifier bound into NOTE_V2 commitments for transfer notes.
    /// - `None` for deposit-style notes without sender_id.
    /// - `Some(hex32)` for transfer outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
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
/// Uses the indexer's `/wallets/:address/balance` endpoint.
///
/// The viewing key is **not** sent to the indexer; the indexer is expected to decrypt using
/// locally-cached viewer keys fetched from `midnight-fvk-service` (admin-token protected).
pub async fn get_privacy_balance(
    provider: &Provider,
    privacy_key: &PrivacyKey,
    viewing_key: Option<&FullViewingKey>,
) -> Result<PrivacyBalanceResult> {
    let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

    tracing::debug!(
        "Fetching privacy pool balance for address {}",
        privacy_address
    );

    // Derive nf_key from privacy key
    let nf_key = privacy_key
        .nf_key(&DOMAIN)
        .ok_or_else(|| anyhow::anyhow!("Privacy key must have spend_sk to derive nf_key"))?;
    let nf_key_hex = hex::encode(nf_key);

    // Viewing key is intentionally not forwarded to the indexer.
    let _ = viewing_key;

    // Call the indexer's balance endpoint
    let balance_response = provider
        .get_wallet_balance(&privacy_address, None, Some(&nf_key_hex), None)
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
                sender_id: note.sender_id,
                tx_hash: note.tx_hash,
                timestamp_ms: note.timestamp_ms,
                kind: note.kind,
            }
        })
        .collect();

    tracing::debug!(
        "Privacy pool balance: {}, unspent notes: {}",
        balance,
        unspent_notes.len()
    );

    // Calculate transaction counts from notes
    let deposit_count = unspent_notes.iter().filter(|n| n.kind == "deposit").count();
    let transfer_count = unspent_notes
        .iter()
        .filter(|n| n.kind == "transfer")
        .count();
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
