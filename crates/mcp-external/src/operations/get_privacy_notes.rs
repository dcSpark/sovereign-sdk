//! Get available (unspent) privacy notes for an address.
//!
//! This is a thin wrapper around the indexer's `/wallets/:address/balance` endpoint,
//! returning the per-note details needed for transaction generation.

use anyhow::{Context, Result};
use midnight_privacy::{recipient_from_pk_v2, FullViewingKey, PrivacyAddress};
use serde::{Deserialize, Serialize};

use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;

const DOMAIN: [u8; 32] = [1u8; 32];

/// Spendable note info as returned by the indexer, normalized for tx generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendableNote {
    pub value: u128,
    /// 32-byte hex (no 0x prefix).
    pub rho: String,
    /// 32-byte hex (no 0x prefix). Always present:
    /// - For deposits: `sender_id = recipient` (deposit convention).
    /// - For transfers: provided in the decrypted plaintext.
    pub sender_id: String,
    pub tx_hash: String,
    pub timestamp_ms: i64,
    pub kind: String,
}

fn normalize_hash32_hex(value: &str) -> Option<String> {
    let normalized = value.trim().trim_start_matches("0x").to_ascii_lowercase();
    let is_hex_32 = normalized.len() == 64 && normalized.chars().all(|c| c.is_ascii_hexdigit());
    if is_hex_32 {
        Some(normalized)
    } else {
        None
    }
}

fn normalize_sender_id_hex(raw_sender_id: &str) -> Option<String> {
    if let Some(hex_sender_id) = normalize_hash32_hex(raw_sender_id) {
        return Some(hex_sender_id);
    }

    // Backward/forward compatibility: some indexer paths may return bech32m recipient strings.
    // Convert those to the NOTE_V2 sender_id hash (recipient hash) expected by tx generation.
    let parsed_addr: PrivacyAddress = raw_sender_id.parse().ok()?;
    let sender_id = recipient_from_pk_v2(&DOMAIN, &parsed_addr.to_pk(), &parsed_addr.pk_ivk());
    Some(hex::encode(sender_id))
}

/// Fetch all available (unspent) notes for `privacy_key`, sorted by value descending.
pub async fn get_privacy_notes(
    provider: &Provider,
    privacy_key: &PrivacyKey,
    viewing_key: Option<&FullViewingKey>,
) -> Result<Vec<SpendableNote>> {
    let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

    // Derive nf_key from privacy key
    let nf_key = privacy_key
        .nf_key(&DOMAIN)
        .ok_or_else(|| anyhow::anyhow!("Privacy key must have spend_sk to derive nf_key"))?;
    let nf_key_hex = hex::encode(nf_key);

    // Viewing key is intentionally not forwarded to the indexer.
    let _ = viewing_key;

    let balance_response = provider
        .get_wallet_balance(&privacy_address, None, Some(&nf_key_hex), None)
        .await
        .context("Failed to fetch balance from indexer")?;

    // Deposit convention: sender_id == recipient (required by NOTE_V2 commitment).
    let deposit_sender_id_hex = hex::encode(privacy_key.recipient(&DOMAIN));

    let mut notes: Vec<SpendableNote> = balance_response
        .unspent_notes
        .into_iter()
        .map(|note| {
            let value = note.value.parse::<u128>().unwrap_or(0);
            let rho = normalize_hash32_hex(&note.rho).unwrap_or_else(|| {
                note.rho
                    .trim()
                    .trim_start_matches("0x")
                    .to_ascii_lowercase()
            });
            let sender_id_raw = note.sender_id.as_deref().unwrap_or(&deposit_sender_id_hex);
            let sender_id =
                normalize_sender_id_hex(sender_id_raw).unwrap_or_else(|| sender_id_raw.to_string());

            SpendableNote {
                value,
                rho,
                sender_id,
                tx_hash: note.tx_hash,
                timestamp_ms: note.timestamp_ms,
                kind: note.kind,
            }
        })
        .collect();

    // Largest-first to maximize value coverage within the input cap (<= 4).
    notes.sort_by(|a, b| {
        b.value
            .cmp(&a.value)
            .then(b.timestamp_ms.cmp(&a.timestamp_ms))
    });

    Ok(notes)
}

/// Select up to `max_inputs` notes, largest-first.
///
/// This matches the tx-generator policy: always use as many notes as possible
/// (up to 4), in descending value order.
pub fn select_largest_notes(
    mut notes: Vec<SpendableNote>,
    max_inputs: usize,
) -> Vec<SpendableNote> {
    notes.sort_by(|a, b| {
        b.value
            .cmp(&a.value)
            .then(b.timestamp_ms.cmp(&a.timestamp_ms))
    });
    notes.truncate(max_inputs);
    notes
}

/// Convenience helper: select up to `max_inputs` notes and ensure the sum covers `send_amount`.
pub fn select_largest_notes_covering_amount(
    notes: Vec<SpendableNote>,
    send_amount: u128,
    max_inputs: usize,
) -> Result<Vec<SpendableNote>> {
    anyhow::ensure!(send_amount > 0, "send_amount must be > 0");
    let selected = select_largest_notes(notes, max_inputs);
    let total_in: u128 = selected.iter().map(|n| n.value).sum();
    anyhow::ensure!(
        total_in >= send_amount,
        "insufficient funds within {} inputs: need {}, have {}",
        max_inputs,
        send_amount,
        total_in
    );
    Ok(selected)
}
