//! Get available (unspent) privacy notes for an address.
//!
//! This is a thin wrapper around the indexer's `/wallets/:address/balance` endpoint,
//! returning the per-note details needed for transaction generation.

use anyhow::{Context, Result};
use midnight_privacy::FullViewingKey;
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
            let rho = note
                .rho
                .trim()
                .trim_start_matches("0x")
                .to_ascii_lowercase();
            let sender_id = note
                .sender_id
                .as_deref()
                .unwrap_or(&deposit_sender_id_hex)
                .trim()
                .trim_start_matches("0x")
                .to_ascii_lowercase();

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

/// Convenience helper: select the fewest largest-first notes needed to cover `send_amount`.
///
/// The selection remains capped by `max_inputs`.
pub fn select_largest_notes_covering_amount(
    notes: Vec<SpendableNote>,
    send_amount: u128,
    max_inputs: usize,
) -> Result<Vec<SpendableNote>> {
    anyhow::ensure!(send_amount > 0, "send_amount must be > 0");
    let notes = select_largest_notes(notes, max_inputs);
    let mut selected: Vec<SpendableNote> = Vec::new();
    let mut total_in: u128 = 0;
    for note in notes {
        total_in = total_in.saturating_add(note.value);
        selected.push(note);
        if total_in >= send_amount {
            break;
        }
    }
    anyhow::ensure!(
        total_in >= send_amount,
        "insufficient funds within {} inputs: need {}, have {}",
        max_inputs,
        send_amount,
        total_in
    );
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::{select_largest_notes_covering_amount, SpendableNote};

    fn note(value: u128, rho_suffix: u8) -> SpendableNote {
        SpendableNote {
            value,
            rho: format!("{:064x}", rho_suffix),
            sender_id: format!("{:064x}", rho_suffix.saturating_add(1)),
            tx_hash: format!("0x{:02x}", rho_suffix),
            timestamp_ms: i64::from(rho_suffix),
            kind: "transfer".to_string(),
        }
    }

    #[test]
    fn covering_amount_uses_minimum_inputs_largest_first() {
        let notes = vec![note(9, 1), note(7, 2), note(3, 3), note(1, 4)];
        let selected =
            select_largest_notes_covering_amount(notes, 8, 4).expect("selection should succeed");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].value, 9);
    }

    #[test]
    fn covering_amount_respects_input_cap() {
        let notes = vec![note(4, 1), note(3, 2), note(2, 3), note(1, 4)];
        let err = select_largest_notes_covering_amount(notes, 10, 2).unwrap_err();
        assert!(
            err.to_string().contains("insufficient funds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn covering_amount_selects_multiple_when_needed() {
        let notes = vec![note(5, 1), note(4, 2), note(3, 3)];
        let selected =
            select_largest_notes_covering_amount(notes, 8, 4).expect("selection should succeed");
        assert_eq!(selected.len(), 2);
        assert_eq!(selected.iter().map(|n| n.value).sum::<u128>(), 9);
    }
}
