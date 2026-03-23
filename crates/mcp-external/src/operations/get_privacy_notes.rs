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

/// Convenience helper: select up to `max_inputs` notes and ensure the sum covers `send_amount`.
pub fn select_largest_notes_covering_amount(
    notes: Vec<SpendableNote>,
    send_amount: u128,
    max_inputs: usize,
) -> Result<Vec<SpendableNote>> {
    const ABI_I64_MAX_U128: u128 = i64::MAX as u128;

    anyhow::ensure!(send_amount > 0, "send_amount must be > 0");
    anyhow::ensure!(max_inputs > 0, "max_inputs must be > 0");
    anyhow::ensure!(
        send_amount <= ABI_I64_MAX_U128,
        "send_amount ({}) exceeds i64 max ({}) required by note_spend_guest v2 ABI",
        send_amount,
        i64::MAX
    );
    let safe_total_upper = send_amount
        .checked_add(ABI_I64_MAX_U128)
        .ok_or_else(|| anyhow::anyhow!("send_amount + i64::MAX overflows u128"))?;

    // `note_spend_guest` v2 encodes note values as i64 in the witness ABI.
    let mut abi_compatible_notes: Vec<SpendableNote> = notes
        .into_iter()
        .filter(|n| n.value <= ABI_I64_MAX_U128)
        .collect();
    abi_compatible_notes.sort_by(|a, b| {
        b.value
            .cmp(&a.value)
            .then(b.timestamp_ms.cmp(&a.timestamp_ms))
    });

    // Max-safe consolidate policy:
    // - Keep largest-first deterministic ordering.
    // - Include up to `max_inputs` notes to consolidate UTXOs.
    // - Never let total exceed `send_amount + i64::MAX`, so change stays ABI-safe.
    let mut selected: Vec<SpendableNote> =
        Vec::with_capacity(max_inputs.min(abi_compatible_notes.len()));
    let mut total_in: u128 = 0;
    for note in abi_compatible_notes.into_iter() {
        if selected.len() >= max_inputs {
            break;
        }
        let candidate_total = total_in
            .checked_add(note.value)
            .ok_or_else(|| anyhow::anyhow!("sum of selected note values overflows u128"))?;
        if candidate_total > safe_total_upper {
            continue;
        }
        total_in = candidate_total;
        selected.push(note);
    }

    if selected.is_empty() {
        anyhow::bail!(
            "no spendable notes compatible with note_spend_guest v2 ABI (value must be <= {})",
            i64::MAX
        );
    }

    if total_in >= send_amount {
        return Ok(selected);
    }

    anyhow::bail!(
        "insufficient spendable funds within {} inputs: need {}, have {}",
        max_inputs,
        send_amount,
        total_in
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_note(value: u128, timestamp_ms: i64, seed: u64) -> SpendableNote {
        SpendableNote {
            value,
            rho: format!("{seed:064x}"),
            sender_id: format!("{:064x}", seed + 1),
            tx_hash: format!("tx-{seed}"),
            timestamp_ms,
            kind: "transfer".to_string(),
        }
    }

    #[test]
    fn covering_selection_uses_extra_inputs_for_consolidation() {
        let notes = vec![
            test_note(90, 3, 1),
            test_note(80, 2, 2),
            test_note(70, 1, 3),
        ];

        let selected = select_largest_notes_covering_amount(notes, 85, 4).unwrap();
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].value, 90);
        assert_eq!(selected[1].value, 80);
        assert_eq!(selected[2].value, 70);
    }

    #[test]
    fn covering_selection_skips_notes_above_i64_max() {
        let cap = i64::MAX as u128;
        let notes = vec![
            test_note(cap + 1, 3, 1),
            test_note(100, 2, 2),
            test_note(90, 1, 3),
        ];

        let selected = select_largest_notes_covering_amount(notes, 100, 4).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].value, 100);
        assert_eq!(selected[1].value, 90);
    }

    #[test]
    fn covering_selection_rejects_send_amount_above_i64_max() {
        let cap = i64::MAX as u128;
        let notes = vec![test_note(cap, 1, 1)];

        let err = select_largest_notes_covering_amount(notes, cap + 1, 4).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("send_amount"));
        assert!(msg.contains("i64 max"));
    }

    #[test]
    fn covering_selection_reports_insufficient_with_input_cap() {
        let notes = vec![
            test_note(50, 3, 1),
            test_note(40, 2, 2),
            test_note(30, 1, 3),
        ];

        let err = select_largest_notes_covering_amount(notes, 95, 2).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("insufficient spendable funds within 2 inputs"));
    }

    #[test]
    fn covering_selection_avoids_oversized_change_while_still_consolidating() {
        let cap = i64::MAX as u128;
        let notes = vec![
            test_note(cap, 3, 1),
            test_note(cap, 2, 2),
            test_note(1, 1, 3),
        ];

        let selected = select_largest_notes_covering_amount(notes, 1, 4).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].value, cap);
        assert_eq!(selected[1].value, 1);
    }

    #[test]
    fn covering_selection_fills_slots_with_later_fitting_notes() {
        let cap = i64::MAX as u128;
        let notes = vec![
            test_note(cap, 3, 1),
            test_note(cap, 2, 2),
            test_note(1, 1, 3),
        ];

        let selected = select_largest_notes_covering_amount(notes, 1, 2).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].value, cap);
        assert_eq!(selected[1].value, 1);
    }
}
