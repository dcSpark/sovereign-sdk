//! Decrypt transaction operation
//!
//! This module provides functionality for decrypting shielded transactions using an authority VFK.

use crate::provider::Provider;
use crate::viewer;
use anyhow::{Context, Result};
use midnight_privacy::{EncryptedNote, Hash32};
use serde::{Deserialize, Serialize};

/// Decrypted note information from a transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptedNote {
    /// Note domain
    pub domain: String,
    /// Token value/amount
    pub value: u128,
    /// Note randomness (rho)
    pub rho: String,
    /// Recipient identifier
    pub recipient: String,
    /// Sender identifier (spender's address for transfers)
    /// - For deposit notes (112 bytes): None
    /// - For transfer notes (144 bytes): Some(sender_id)
    pub sender_id: Option<String>,
}

/// Result of decrypting a transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptTransactionResult {
    /// Transaction hash
    pub tx_hash: String,
    /// Transaction status
    pub status: String,
    /// Transaction kind
    pub kind: Option<String>,
    /// Timestamp in milliseconds
    pub timestamp_ms: Option<i64>,
    /// Decrypted notes from the transaction
    pub decrypted_notes: Vec<DecryptedNote>,
    /// Number of encrypted notes that were successfully decrypted
    pub decrypted_count: usize,
    /// Total number of encrypted notes in the transaction
    pub total_encrypted_notes: usize,
}

/// Decrypt a transaction using the authority VFK
///
/// This operation:
/// 1. Fetches the transaction from the indexer
/// 2. Extracts encrypted notes from the transaction
/// 3. Decrypts each note using the provided VFK
/// 4. Returns decrypted note data along with transaction metadata
///
/// # Parameters
/// * `provider` - Provider for indexer connection
/// * `tx_hash` - Transaction hash to decrypt
/// * `vfk` - Authority VFK (32-byte hex string, with or without 0x prefix)
///
/// # Returns
/// DecryptTransactionResult containing decrypted notes and transaction metadata
///
/// # Example
/// ```rust,no_run
/// # async fn example(provider: &mcp_external::provider::Provider) -> anyhow::Result<()> {
/// use mcp_external::operations::decrypt_transaction;
///
/// let vfk_hex = "0x1234..."; // Authority VFK
/// let result = decrypt_transaction(provider, "0xabcd...", vfk_hex).await?;
/// println!("Decrypted {} of {} notes", result.decrypted_count, result.total_encrypted_notes);
/// # Ok(())
/// # }
/// ```
#[allow(dead_code)]
pub async fn decrypt_transaction(
    provider: &Provider,
    tx_hash: &str,
    vfk_hex: &str,
) -> Result<DecryptTransactionResult> {
    tracing::info!("Fetching transaction {} for decryption", tx_hash);

    // Parse VFK from hex string
    let vfk = parse_vfk_hex(vfk_hex).context("Failed to parse VFK hex string")?;

    // Fetch transaction from indexer
    let tx_option = provider
        .get_transaction(tx_hash)
        .await
        .with_context(|| format!("Failed to fetch transaction {}", tx_hash))?;

    let tx =
        tx_option.ok_or_else(|| anyhow::anyhow!("Transaction {} not found in indexer", tx_hash))?;

    tracing::info!(
        "Transaction fetched: kind={}, status={:?}",
        tx.kind,
        tx.status
    );

    // Extract encrypted notes from the transaction's encrypted_notes field
    let encrypted_notes = extract_encrypted_notes_from_tx(&tx.encrypted_notes)
        .context("Failed to extract encrypted notes from transaction")?;

    let total_encrypted_notes = encrypted_notes.len();
    tracing::info!(
        "Found {} encrypted notes in transaction",
        total_encrypted_notes
    );

    // Decrypt each note
    let mut decrypted_notes = Vec::new();
    for (idx, encrypted_note) in encrypted_notes.iter().enumerate() {
        match viewer::decrypt_note(&vfk, encrypted_note) {
            Ok((domain, value, rho, recipient, sender_id)) => {
                let note_type = if sender_id.is_some() {
                    "transfer"
                } else {
                    "deposit"
                };
                tracing::info!(
                    "Successfully decrypted {} note {}: value={}",
                    note_type,
                    idx,
                    value
                );
                decrypted_notes.push(DecryptedNote {
                    domain: hex::encode(domain),
                    value,
                    rho: hex::encode(rho),
                    recipient: hex::encode(recipient),
                    sender_id: sender_id.map(|s| hex::encode(s)),
                });
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to decrypt note {}: {} (note may not be encrypted for this VFK)",
                    idx,
                    e
                );
                // Continue trying to decrypt other notes
            }
        }
    }

    let decrypted_count = decrypted_notes.len();
    tracing::info!(
        "Decrypted {}/{} notes using provided VFK",
        decrypted_count,
        total_encrypted_notes
    );

    Ok(DecryptTransactionResult {
        tx_hash: tx.tx_hash,
        status: tx.status.unwrap_or_else(|| "Unknown".to_string()),
        kind: Some(tx.kind),
        timestamp_ms: Some(tx.timestamp_ms),
        decrypted_notes,
        decrypted_count,
        total_encrypted_notes,
    })
}

/// Parse a VFK from a hex string (with or without 0x prefix)
#[allow(dead_code)]
fn parse_vfk_hex(vfk_hex: &str) -> Result<Hash32> {
    let s = vfk_hex.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).with_context(|| format!("Invalid hex string: {}", vfk_hex))?;

    if bytes.len() != 32 {
        anyhow::bail!("VFK must be exactly 32 bytes, got {} bytes", bytes.len());
    }

    let mut vfk = [0u8; 32];
    vfk.copy_from_slice(&bytes);
    Ok(vfk)
}

/// Extract encrypted notes from the transaction's encrypted_notes field
#[allow(dead_code)]
fn extract_encrypted_notes_from_tx(
    encrypted_notes_field: &Option<serde_json::Value>,
) -> Result<Vec<EncryptedNote>> {
    let encrypted_notes_value = match encrypted_notes_field {
        Some(val) => val,
        None => {
            tracing::debug!("Transaction has no encrypted_notes field");
            return Ok(Vec::new());
        }
    };

    // The encrypted_notes field should be an array of EncryptedNote objects
    let arr = match encrypted_notes_value.as_array() {
        Some(arr) => arr,
        None => {
            tracing::warn!(
                "encrypted_notes field is not an array: {:?}",
                encrypted_notes_value
            );
            return Ok(Vec::new());
        }
    };

    let mut encrypted_notes = Vec::new();
    for (idx, item) in arr.iter().enumerate() {
        match serde_json::from_value::<EncryptedNote>(item.clone()) {
            Ok(note) => {
                tracing::debug!("Successfully parsed encrypted note {}", idx);
                encrypted_notes.push(note);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to parse encrypted note {}: {} - value: {:?}",
                    idx,
                    e,
                    item
                );
            }
        }
    }

    if encrypted_notes.is_empty() {
        tracing::warn!("No valid encrypted notes found in transaction");
    }

    Ok(encrypted_notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vfk_hex() {
        // Test with 0x prefix
        let vfk_hex = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let result = parse_vfk_hex(vfk_hex);
        assert!(result.is_ok());

        // Test without 0x prefix
        let vfk_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let result = parse_vfk_hex(vfk_hex);
        assert!(result.is_ok());

        // Test invalid length
        let vfk_hex = "0x0123";
        let result = parse_vfk_hex(vfk_hex);
        assert!(result.is_err());

        // Test invalid hex
        let vfk_hex = "0xGGGG";
        let result = parse_vfk_hex(vfk_hex);
        assert!(result.is_err());
    }
}
