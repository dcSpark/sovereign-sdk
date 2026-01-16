//! Verify transaction operation
//!
//! This module provides functionality for verifying if a transaction exists
//! and extracting its amount (decrypting if possible).

use crate::provider::Provider;
use crate::viewer;
use anyhow::{Context, Result};
use midnight_privacy::{EncryptedNote, Hash32};
use serde::{Deserialize, Serialize};

/// Sync status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    /// Indices that have been synced
    #[serde(rename = "syncedIndices")]
    pub synced_indices: String,
    /// Lag information
    pub lag: LagInfo,
    /// Whether the wallet is fully synced
    #[serde(rename = "isFullySynced")]
    pub is_fully_synced: bool,
}

/// Lag information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LagInfo {
    /// Apply gap value
    #[serde(rename = "applyGap")]
    pub apply_gap: String,
    /// Source gap value
    #[serde(rename = "sourceGap")]
    pub source_gap: String,
}

/// Result of verifying a transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyTransactionResult {
    /// Whether the transaction exists in the wallet
    pub exists: bool,
    /// Current sync status information
    #[serde(rename = "syncStatus")]
    pub sync_status: SyncStatus,
    /// The amount of the transaction (in dust format), or "encrypted" if cannot decrypt
    #[serde(rename = "transactionAmount")]
    pub transaction_amount: String,
}

/// Verify if a transaction exists and extract its amount
///
/// This operation:
/// 1. Fetches the transaction from the indexer
/// 2. Attempts to decrypt the transaction to get the amount
/// 3. If decryption fails or no FVK is provided, returns "encrypted"
/// 4. Returns sync status (mocked as fully synced)
///
/// **Important**: For transfer transactions with multiple encrypted notes, this returns
/// the FIRST successfully decrypted note's value (the sent amount), NOT the sum of all notes.
/// This is correct because:
/// - For deposits: only one note exists (the deposited amount)
/// - For transfers: Output 0 (first note) is the sent amount, Output 1 is change back to sender
///
/// # Parameters
/// * `provider` - Provider for indexer connection
/// * `identifier` - Transaction hash to verify
/// * `fvk_hex` - Optional authority FVK (32-byte hex string, with or without 0x prefix)
///
/// # Returns
/// VerifyTransactionResult containing existence status, sync status, and transaction amount
///
/// # Example
/// ```rust,no_run
/// # async fn example(provider: &mcp_external::provider::Provider) -> anyhow::Result<()> {
/// use mcp_external::operations::verify_transaction;
///
/// let result = verify_transaction(provider, "0xabcd...", Some("0x1234...")).await?;
/// println!("Transaction exists: {}", result.exists);
/// println!("Amount: {}", result.transaction_amount);
/// # Ok(())
/// # }
/// ```
pub async fn verify_transaction(
    provider: &Provider,
    identifier: &str,
    fvk_hex: Option<&str>,
) -> Result<VerifyTransactionResult> {
    tracing::info!("Verifying transaction {}", identifier);

    // Try to fetch transaction from indexer
    let tx_option = provider
        .get_transaction(identifier)
        .await
        .with_context(|| format!("Failed to fetch transaction {}", identifier))?;

    // Check if transaction exists
    let exists = tx_option.is_some();

    if !exists {
        tracing::info!("Transaction {} not found", identifier);
        return Ok(VerifyTransactionResult {
            exists: false,
            sync_status: create_mock_sync_status(),
            transaction_amount: "0".to_string(),
        });
    }

    let tx = tx_option.unwrap();
    tracing::info!(
        "Transaction found: kind={}, status={:?}",
        tx.kind,
        tx.status
    );

    // Try to extract amount from the transaction
    let transaction_amount = if let Some(fvk) = fvk_hex {
        // Try to decrypt the transaction to get the amount
        extract_amount_from_transaction(&tx.encrypted_notes, fvk).await
    } else {
        // No FVK provided, check if there's a plaintext amount field
        if let Some(amount_str) = &tx.amount {
            tracing::info!("Using plaintext amount from transaction: {}", amount_str);
            amount_str.clone()
        } else {
            tracing::info!("No FVK provided and no plaintext amount, marking as encrypted");
            "encrypted".to_string()
        }
    };

    Ok(VerifyTransactionResult {
        exists: true,
        sync_status: create_mock_sync_status(),
        transaction_amount,
    })
}

/// Extract amount from encrypted notes by attempting to decrypt them
///
/// Returns the FIRST successfully decrypted note's value, not the sum of all notes.
/// This is the correct behavior for verifying the sent amount in transfers.
async fn extract_amount_from_transaction(
    encrypted_notes_field: &Option<serde_json::Value>,
    fvk_hex: &str,
) -> String {
    // Try to parse FVK
    let fvk = match parse_fvk_hex(fvk_hex) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse FVK: {}, marking amount as encrypted", e);
            return "encrypted".to_string();
        }
    };

    // Extract encrypted notes
    let encrypted_notes = match extract_encrypted_notes_from_tx(encrypted_notes_field) {
        Ok(notes) => notes,
        Err(e) => {
            tracing::warn!(
                "Failed to extract encrypted notes: {}, marking amount as encrypted",
                e
            );
            return "encrypted".to_string();
        }
    };

    if encrypted_notes.is_empty() {
        tracing::info!("No encrypted notes found, marking amount as encrypted");
        return "encrypted".to_string();
    }

    // Decrypt notes and return the FIRST successfully decrypted note's value.
    // This is correct because:
    // - For deposits: only one note exists (the deposited amount)
    // - For transfers: Output 0 (first note) is always the sent amount, Output 1 is change
    // We should NOT sum all notes, as that would return the original note value instead of the sent amount.

    for (idx, encrypted_note) in encrypted_notes.iter().enumerate() {
        match viewer::decrypt_note(&fvk, encrypted_note) {
            Ok((_domain, value, _rho, _recipient, _sender_id)) => {
                tracing::info!(
                    "Successfully decrypted note {}: value={} (using this as transaction amount)",
                    idx,
                    value
                );
                return value.to_string();
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to decrypt note {}: {} (note may not be encrypted for this FVK)",
                    idx,
                    e
                );
            }
        }
    }

    tracing::info!("Could not decrypt any notes, marking amount as encrypted");
    "encrypted".to_string()
}

/// Parse a FVK from a hex string (with or without 0x prefix)
fn parse_fvk_hex(fvk_hex: &str) -> Result<Hash32> {
    let s = fvk_hex.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).with_context(|| format!("Invalid hex string: {}", fvk_hex))?;

    if bytes.len() != 32 {
        anyhow::bail!("FVK must be exactly 32 bytes, got {} bytes", bytes.len());
    }

    let mut fvk = [0u8; 32];
    fvk.copy_from_slice(&bytes);
    Ok(fvk)
}

/// Extract encrypted notes from the transaction's encrypted_notes field
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
        tracing::debug!("No valid encrypted notes found in transaction");
    }

    Ok(encrypted_notes)
}

/// Create mock sync status (always fully synced)
fn create_mock_sync_status() -> SyncStatus {
    SyncStatus {
        synced_indices: "all".to_string(),
        lag: LagInfo {
            apply_gap: "0".to_string(),
            source_gap: "0".to_string(),
        },
        is_fully_synced: true,
    }
}
