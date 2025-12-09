//! Get Privacy Pool Balance Operation
//!
//! This operation scans all transactions from the indexer to calculate the user's
//! privacy pool balance by:
//! 1. Finding notes that belong to the user (by decrypting and matching recipient)
//! 2. Tracking which notes have been spent (by matching nullifiers)
//! 3. Computing the balance as the sum of unspent note values

use std::collections::HashSet;

use anyhow::{Context, Result};
use midnight_privacy::{nullifier, EncryptedNote, FullViewingKey, Hash32};
use serde::{Deserialize, Serialize};

use crate::privacy_key::PrivacyKey;
use crate::provider::{InvolvementItem, Provider};

/// Domain constant matching the one used in deposit.rs/transfer.rs
const DOMAIN: [u8; 32] = [1u8; 32];

/// Legacy NF key used by the initial transfer implementation (hardcoded).
/// We try this as a fallback when matching nullifiers to avoid overreporting balance
/// for older transfers that did not use the user's derived nf_key.
const LEGACY_NF_KEY: [u8; 32] = [4u8; 32];

/// Default page size for fetching transactions
const DEFAULT_PAGE_SIZE: usize = 100;

/// An unspent note in the privacy pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnspentNote {
    /// Note value
    pub value: u128,
    /// Note rho (nonce)
    pub rho: String,
    /// Transaction hash where this note was created
    pub tx_hash: String,
    /// Timestamp when the note was created (milliseconds)
    pub timestamp_ms: i64,
    /// Transaction kind (deposit, transfer, withdraw)
    pub kind: String,
}

/// Privacy pool balance result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyBalanceResult {
    /// Total unspent balance in the privacy pool
    pub balance: u128,
    /// List of unspent notes
    pub unspent_notes: Vec<UnspentNote>,
    /// Number of deposits received
    pub deposit_count: usize,
    /// Number of transfers received
    pub transfer_count: usize,
    /// Number of withdrawals made
    pub withdraw_count: usize,
    /// Total transactions scanned
    pub total_transactions_scanned: usize,
}

/// Get the privacy pool balance for the user
///
/// This operation:
/// 1. Derives the user's recipient and nf_key from their privacy key
/// 2. Pages through all transactions from the indexer
/// 3. For each transaction:
///    - Tries to decrypt encrypted notes with the viewing key
///    - Checks if the decrypted note's recipient matches the user's recipient
///    - If yes, records the note as owned by the user
/// 4. For each owned note, checks if it's been spent by matching its nullifier
/// 5. Returns the sum of unspent notes and detailed note information
///
/// # Parameters
/// * `provider` - Provider for fetching transactions from indexer
/// * `privacy_key` - User's privacy key for deriving recipient and nf_key
/// * `viewing_key` - Viewing key for decrypting notes (FVK)
///
/// # Returns
/// Privacy balance result with balance, unspent notes, and transaction counts
pub async fn get_privacy_balance(
    provider: &Provider,
    privacy_key: &PrivacyKey,
    viewing_key: &FullViewingKey,
) -> Result<PrivacyBalanceResult> {
    tracing::info!("Starting privacy pool balance calculation");

    // Derive user's recipient and nf_key from privacy key
    let user_recipient = privacy_key.recipient(&DOMAIN);
    let user_nf_key = privacy_key
        .nf_key(&DOMAIN)
        .ok_or_else(|| anyhow::anyhow!("Privacy key must have spend_sk to calculate balance"))?;

    tracing::info!(
        "User recipient: {}, nf_key: {}",
        hex::encode(&user_recipient),
        hex::encode(&user_nf_key)
    );

    // Track all notes (both unspent and spent)
    let mut all_notes: Vec<(Hash32, u128, String, i64, String)> = Vec::new(); // (rho, value, tx_hash, timestamp, kind)
    let mut seen_rhos: HashSet<Hash32> = HashSet::new();
    let mut spent_nullifiers: HashSet<String> = HashSet::new();
    let mut transfer_nullifiers: HashSet<String> = HashSet::new();

    let mut deposit_count = 0;
    let mut transfer_incoming_count = 0;
    let mut transfer_outgoing_count = 0;
    let mut withdraw_count = 0;
    let mut total_transactions = 0;

    // Helper to record a note once (dedup by rho) and push into the accumulator.
    let mut record_note = |rho: Hash32, value: u128, tx: &InvolvementItem| -> bool {
        if seen_rhos.insert(rho) {
            all_notes.push((
                rho,
                value,
                tx.tx_hash.clone(),
                tx.timestamp_ms,
                tx.kind.clone(),
            ));
            true
        } else {
            false
        }
    };

    // Page through all transactions
    let mut offset = 0;
    loop {
        tracing::debug!("Fetching transactions at offset {}", offset);

        let tx_list = provider
            .get_all_transactions(Some(DEFAULT_PAGE_SIZE), Some(offset))
            .await
            .context("Failed to fetch transactions from indexer")?;

        if tx_list.items.is_empty() {
            tracing::info!("Reached end of transactions at offset {}", offset);
            break;
        }

        total_transactions += tx_list.items.len();
        tracing::debug!("Processing {} transactions", tx_list.items.len());

        for tx in &tx_list.items {
            // Check if this tx has a nullifier (means a note was spent)
            if let Some(ref nullifier_str) = tx.nullifier {
                // Normalize nullifier (remove 0x prefix, lowercase)
                let normalized_nullifier = nullifier_str
                    .strip_prefix("0x")
                    .unwrap_or(nullifier_str)
                    .to_lowercase();
                spent_nullifiers.insert(normalized_nullifier.clone());
                if tx.kind == "transfer" {
                    transfer_nullifiers.insert(normalized_nullifier);
                }
            }

            // Deposits include all the data we need in the payload; parse it regardless of
            // whether encrypted notes are present so we don't depend on viewer ciphertexts.
            if tx.kind == "deposit" {
                if let Some(ref payload) = tx.payload {
                    match (
                        extract_recipient_from_deposit_payload(payload),
                        extract_rho_from_deposit_payload(payload),
                    ) {
                        (Ok(recipient_from_payload), Ok(rho))
                            if recipient_from_payload == user_recipient =>
                        {
                            let value = tx
                                .amount
                                .as_ref()
                                .and_then(|s| s.parse::<u128>().ok())
                                .or_else(|| extract_amount_from_deposit_payload(payload));

                            if let Some(value) = value {
                                if record_note(rho, value, tx) {
                                    deposit_count += 1;
                                }
                            } else {
                                tracing::trace!(
                                    tx_hash = tx.tx_hash.as_str(),
                                    "Deposit payload missing parsable amount"
                                );
                            }
                        }
                        (Err(e_recipient), _) => {
                            tracing::trace!(
                                tx_hash = tx.tx_hash.as_str(),
                                "Failed to parse deposit recipient from payload: {}",
                                e_recipient
                            );
                        }
                        (_, Err(e_rho)) => {
                            tracing::trace!(
                                tx_hash = tx.tx_hash.as_str(),
                                "Failed to parse deposit rho from payload: {}",
                                e_rho
                            );
                        }
                        _ => {}
                    }
                }
            }

            // Try to find notes belonging to the user
            // First, try to decrypt encrypted_notes
            if let Some(ref encrypted_notes_json) = tx.encrypted_notes {
                if let Ok(encrypted_notes) =
                    serde_json::from_value::<Vec<EncryptedNote>>(encrypted_notes_json.clone())
                {
                    for encrypted_note in encrypted_notes {
                        // Try to decrypt with viewing key
                        match decrypt_note(&encrypted_note, viewing_key) {
                            Ok((value, rho, recipient)) => {
                                // Check if recipient matches user's recipient
                                if recipient == user_recipient {
                                    tracing::debug!(
                                        "Found note belonging to user: value={}, rho={}, tx={}",
                                        value,
                                        hex::encode(&rho),
                                        tx.tx_hash
                                    );
                                    if record_note(rho, value, tx) {
                                        // Count by type only when we add a new note
                                        match tx.kind.as_str() {
                                            "deposit" => deposit_count += 1,
                                            "transfer" => transfer_incoming_count += 1,
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::trace!("Failed to decrypt note: {}", e);
                                // Not our note, continue
                            }
                        }
                    }
                }
            }

            // Count withdrawals (we spent a note)
            if tx.kind == "withdraw" && tx.nullifier.is_some() {
                withdraw_count += 1;
            }
        }

        offset += tx_list.items.len();

        // Check if we've reached the end (no more items or less than page size)
        if tx_list.items.len() < DEFAULT_PAGE_SIZE {
            break;
        }
    }

    tracing::info!(
        "Scanned {} transactions, found {} notes",
        total_transactions,
        all_notes.len()
    );

    // Now filter out spent notes
    let mut unspent_notes = Vec::new();
    let mut balance: u128 = 0;

    for (rho, value, tx_hash, timestamp_ms, kind) in all_notes {
        // Compute the nullifier for this note
        let note_nullifier_user = nullifier(&DOMAIN, &user_nf_key, &rho);
        let nullifier_hex_user = hex::encode(&note_nullifier_user);

        // Fallback to legacy NF key for earlier transfers
        let note_nullifier_legacy = nullifier(&DOMAIN, &LEGACY_NF_KEY, &rho);
        let nullifier_hex_legacy = hex::encode(&note_nullifier_legacy);

        // Check if this note has been spent
        let spent_by_user_nf = spent_nullifiers.contains(&nullifier_hex_user);
        let spent_by_legacy_nf = spent_nullifiers.contains(&nullifier_hex_legacy);
        let spent_by_transfer = transfer_nullifiers.contains(&nullifier_hex_user)
            || transfer_nullifiers.contains(&nullifier_hex_legacy);

        if spent_by_user_nf || spent_by_legacy_nf {
            tracing::debug!("Note {} has been spent", hex::encode(&rho));
            if spent_by_transfer {
                transfer_outgoing_count += 1;
            }
        } else {
            // Note is unspent
            tracing::debug!("Note {} is unspent, value={}", hex::encode(&rho), value);
            balance += value;
            unspent_notes.push(UnspentNote {
                value,
                rho: hex::encode(&rho),
                tx_hash,
                timestamp_ms,
                kind,
            });
        }
    }

    tracing::info!(
        "Privacy pool balance: {}, unspent notes: {}",
        balance,
        unspent_notes.len()
    );

    Ok(PrivacyBalanceResult {
        balance,
        unspent_notes,
        deposit_count,
        transfer_count: transfer_incoming_count + transfer_outgoing_count,
        withdraw_count,
        total_transactions_scanned: total_transactions,
    })
}

/// Decrypt an encrypted note with a viewing key
///
/// Returns (value, rho, recipient) if successful
fn decrypt_note(
    encrypted_note: &EncryptedNote,
    viewing_key: &FullViewingKey,
) -> Result<(u128, Hash32, Hash32)> {
    // Decrypt and verify the note using Level B encryption
    let note =
        midnight_privacy::viewing::decrypt_and_verify_note_level_b(viewing_key, encrypted_note)
            .context("Failed to decrypt note")?;

    Ok((note.value, note.rho, note.recipient))
}

/// Extract recipient from deposit payload
///
/// Deposit payload can come from the verifier (`{ "deposit": { ... } }`) or from
/// the module wrapper (`{ "MidnightPrivacy": { "Deposit": { ... } } }`).
fn extract_deposit_field<'a>(
    payload: &'a serde_json::Value,
    field: &str,
) -> Option<&'a serde_json::Value> {
    payload
        .get("MidnightPrivacy")
        .and_then(|mp| mp.get("Deposit"))
        .and_then(|dep| dep.get(field))
        .or_else(|| payload.get("deposit").and_then(|dep| dep.get(field)))
}

/// Parse a 32-byte hash from common payload encodings (hex string, bracketed byte string, or JSON array).
fn parse_hash32_from_value(value: &serde_json::Value, field_name: &str) -> Result<Hash32> {
    // String encodings: hex ("0x...") or "[1, 2, ...]"
    if let Some(s) = value.as_str() {
        if let Some(parsed) = parse_hash32_string(s) {
            return Ok(parsed);
        }
    }

    // Array encoding: [1, 2, ...]
    if let Some(arr) = value.as_array() {
        let mut bytes = Vec::with_capacity(arr.len());
        for item in arr {
            if let Some(n) = item.as_u64() {
                if n > u8::MAX as u64 {
                    anyhow::bail!("{field_name} byte value exceeds u8 range");
                }
                bytes.push(n as u8);
            } else if let Some(s) = item.as_str() {
                let n: u64 = s
                    .trim()
                    .parse()
                    .with_context(|| format!("Invalid {field_name} byte value"))?;
                if n > u8::MAX as u64 {
                    anyhow::bail!("{field_name} byte value exceeds u8 range");
                }
                bytes.push(n as u8);
            } else {
                anyhow::bail!("Unsupported {field_name} encoding in array");
            }
        }

        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Ok(out);
        }
    }

    anyhow::bail!("{field_name} must be 32 bytes in hex or byte array format")
}

/// Parse a 32-byte hash from a hex string or bracketed decimal byte list.
fn parse_hash32_string(s: &str) -> Option<Hash32> {
    let trimmed = s.trim();

    // Hex encoding
    let hex_str = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if let Ok(bytes) = hex::decode(hex_str) {
        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Some(out);
        }
    }

    // Bracketed decimal encoding: "[1, 2, 3, ...]"
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let mut bytes = Vec::new();
        for part in inner.split(',') {
            let piece = part.trim();
            if piece.is_empty() {
                continue;
            }
            let Ok(n) = piece.parse::<u8>() else {
                return None;
            };
            bytes.push(n);
        }

        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Some(out);
        }
    }

    None
}

/// Extract recipient from deposit payload
fn extract_recipient_from_deposit_payload(payload: &serde_json::Value) -> Result<Hash32> {
    let value = extract_deposit_field(payload, "recipient")
        .ok_or_else(|| anyhow::anyhow!("No recipient in deposit payload"))?;
    parse_hash32_from_value(value, "recipient")
}

/// Extract rho from deposit payload
fn extract_rho_from_deposit_payload(payload: &serde_json::Value) -> Result<Hash32> {
    let value = extract_deposit_field(payload, "rho")
        .ok_or_else(|| anyhow::anyhow!("No rho in deposit payload"))?;
    parse_hash32_from_value(value, "rho")
}

/// Extract amount from deposit payload
fn extract_amount_from_deposit_payload(payload: &serde_json::Value) -> Option<u128> {
    let value = extract_deposit_field(payload, "amount")?;
    if let Some(s) = value.as_str() {
        return s.parse::<u128>().ok();
    }
    value.as_u64().map(|v| v as u128)
}
