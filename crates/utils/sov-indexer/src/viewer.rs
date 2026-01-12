//! VFK Registry and decryption support for the indexer
//!
//! Supports multiple VFKs loaded from a config file. Each address has its own VFK,
//! and the indexer looks up the correct VFK based on the fvk_commitment in each note.
//!
//! Uses DashMap for lock-free concurrent access during indexing.

use crate::index_db as idx;
use anyhow::Result;
use bech32::{Bech32m, Hrp};
use chrono::Utc;
use dashmap::DashMap;
use midnight_privacy::{
    viewing::{ct_hash, fvk_commitment, view_kdf, view_mac},
    EncryptedNote, FullViewingKey, Hash32,
};
use sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{debug, info, warn};

/// Human-readable prefix for privacy pool addresses
pub const PRIVACY_ADDRESS_HRP: &str = "privpool";

/// Length of note plaintext for deposits: 32(domain) + 16(value) + 32(rho) + 32(recipient)
pub const NOTE_PLAIN_LEN_DEPOSIT: usize = 112;

/// Length of note plaintext for transfers: 32(domain) + 16(value) + 32(rho) + 32(recipient) + 32(sender_id)
pub const NOTE_PLAIN_LEN_TRANSFER: usize = 144;

/// Decrypted note data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptedNote {
    pub domain: String,
    pub value: String,
    pub rho: String,
    pub recipient: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
}

/// A single VFK entry from the config file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfkEntry {
    /// The VFK as hex string (32 bytes = 64 hex chars)
    pub vfk: String,
    /// The shielded address associated with this VFK (optional)
    #[serde(default)]
    pub shielded_address: Option<String>,
}

/// Config file structure for VFK registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfkConfig {
    /// List of VFK entries
    pub vfks: Vec<VfkEntry>,
}

/// Registry of VFKs indexed by their commitment for fast concurrent lookup.
/// Uses DashMap for lock-free concurrent access during indexing.
#[derive(Debug)]
pub struct VfkRegistry {
    /// Map from fvk_commitment (hex) -> (vfk bytes, shielded_address)
    by_commitment: DashMap<String, (Hash32, Option<String>)>,
}

impl VfkRegistry {
    /// Create an empty registry
    pub fn new() -> Self {
        Self {
            by_commitment: DashMap::new(),
        }
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.by_commitment.is_empty()
    }

    /// Number of VFKs in the registry
    pub fn len(&self) -> usize {
        self.by_commitment.len()
    }

    /// Add a VFK to the registry (thread-safe, no &mut needed)
    pub fn add(&self, vfk: Hash32, shielded_address: Option<String>) {
        let vfk_obj = FullViewingKey(vfk);
        let commitment = fvk_commitment(&vfk_obj);
        let commitment_hex = hex::encode(commitment);
        self.by_commitment
            .insert(commitment_hex, (vfk, shielded_address));
    }

    /// Remove a VFK by its commitment (thread-safe)
    pub fn remove(&self, commitment_hex: &str) -> bool {
        self.by_commitment.remove(commitment_hex).is_some()
    }

    /// Look up a VFK by its commitment (hex string) and return a copy
    pub fn get_vfk(&self, commitment_hex: &str) -> Option<Hash32> {
        self.by_commitment.get(commitment_hex).map(|r| r.0)
    }

    /// Check if a commitment exists in the registry
    #[allow(unused)]
    pub fn contains(&self, commitment_hex: &str) -> bool {
        self.by_commitment.contains_key(commitment_hex)
    }

    /// Get all entries as a collected Vec (for iteration/serialization)
    pub fn entries(&self) -> Vec<(String, Hash32, Option<String>)> {
        self.by_commitment
            .iter()
            .map(|r| {
                let (k, (vfk, addr)) = r.pair();
                (k.clone(), *vfk, addr.clone())
            })
            .collect()
    }

    /// Load VFKs from a JSON config file
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read VFK config file {:?}: {}", path, e))?;

        let config: VfkConfig = serde_json::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("Failed to parse VFK config file {:?}: {}", path, e))?;

        let registry = Self::new();

        for entry in config.vfks {
            let vfk = parse_vfk_hex(&entry.vfk)?;
            registry.add(vfk, entry.shielded_address);
        }

        info!("Loaded {} VFKs from config file {:?}", registry.len(), path);
        Ok(registry)
    }

    /// Save the registry to the database
    pub async fn save_to_db(&self, db: &DatabaseConnection) -> Result<()> {
        use sea_orm::sea_query::OnConflict;

        for entry in self.by_commitment.iter() {
            let (commitment_hex, (vfk, shielded_address)) = entry.pair();
            let model = idx::vfk_registry::ActiveModel {
                fvk_commitment: Set(commitment_hex.clone()),
                vfk: Set(hex::encode(vfk)),
                shielded_address: Set(shielded_address.clone()),
                created_at: Set(Utc::now()),
            };

            idx::vfk_registry::Entity::insert(model)
                .on_conflict(
                    OnConflict::column(idx::vfk_registry::Column::FvkCommitment)
                        .update_columns([
                            idx::vfk_registry::Column::Vfk,
                            idx::vfk_registry::Column::ShieldedAddress,
                        ])
                        .to_owned(),
                )
                .exec(db)
                .await?;
        }

        info!("Saved {} VFKs to database", self.len());
        Ok(())
    }

    /// Load the registry from the database
    pub async fn load_from_db(db: &DatabaseConnection) -> Result<Self> {
        let rows = idx::vfk_registry::Entity::find().all(db).await?;

        let registry = Self::new();

        for row in rows {
            let vfk = parse_vfk_hex(&row.vfk)?;
            // We already have the commitment stored, but we re-add to populate our DashMap
            registry.add(vfk, row.shielded_address);
        }

        if !registry.is_empty() {
            info!("Loaded {} VFKs from database", registry.len());
        }

        Ok(registry)
    }
}

impl Default for VfkRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a VFK from a hex string (with or without 0x prefix)
pub fn parse_vfk_hex(vfk_hex: &str) -> Result<Hash32> {
    let s = vfk_hex.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes =
        hex::decode(s).map_err(|e| anyhow::anyhow!("Invalid VFK hex '{}': {}", vfk_hex, e))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "VFK must be 32 bytes (64 hex chars), got {} bytes",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Load authority viewing key from environment variable AUTHORITY_VFK.
/// This is for backward compatibility with single-VFK mode.
pub fn load_authority_vfk() -> Option<Hash32> {
    let raw = std::env::var("AUTHORITY_VFK").ok()?;
    match parse_vfk_hex(&raw) {
        Ok(vfk) => Some(vfk),
        Err(e) => {
            warn!("AUTHORITY_VFK is set but invalid: {}", e);
            None
        }
    }
}

/// Load VFK config file path from environment variable VFK_CONFIG_FILE
pub fn load_vfk_config_path() -> Option<std::path::PathBuf> {
    std::env::var("VFK_CONFIG_FILE")
        .ok()
        .map(std::path::PathBuf::from)
}

/// Produce the i-th 32-byte stream block for key k using Poseidon2.
fn stream_block(k: &Hash32) -> impl Fn(u32) -> Hash32 + '_ {
    move |ctr: u32| {
        let c = ctr.to_le_bytes();
        midnight_privacy::poseidon2_hash(b"VIEW_STREAM_V1", &[k, &c])
    }
}

/// SNARK-friendly deterministic decryption: XOR ciphertext with Poseidon-based keystream.
fn stream_xor_decrypt(k: &Hash32, ct: &[u8], pt_out: &mut [u8]) {
    debug_assert_eq!(ct.len(), pt_out.len());
    let block_fn = stream_block(k);
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < ct.len() {
        let ks = block_fn(ctr);
        ctr = ctr.wrapping_add(1);
        let take = core::cmp::min(32, ct.len() - off);
        for i in 0..take {
            pt_out[off + i] = ct[off + i] ^ ks[i];
        }
        off += take;
    }
}

/// Decrypt an encrypted note using the provided VFK.
///
/// Supports both deposit notes (112 bytes, no sender_id) and transfer notes (144 bytes, with sender_id).
pub fn decrypt_note(vfk: &Hash32, encrypted_note: &EncryptedNote) -> Result<DecryptedNote> {
    let vfk_obj = FullViewingKey(*vfk);
    let expected_vfk_c = fvk_commitment(&vfk_obj);

    // Verify VFK commitment matches
    if encrypted_note.fvk_commitment != expected_vfk_c {
        anyhow::bail!("VFK commitment mismatch: note is not encrypted for this viewing key");
    }

    // Derive decryption key
    let k = view_kdf(&vfk_obj, &encrypted_note.cm);

    // Verify MAC before decryption
    let ct_h = ct_hash(encrypted_note.ct.as_ref());
    let expected_mac = view_mac(&k, &encrypted_note.cm, &ct_h);
    if encrypted_note.mac != expected_mac {
        anyhow::bail!("MAC verification failed: ciphertext may be corrupted");
    }

    // Decrypt ciphertext - support both 112-byte (deposit) and 144-byte (transfer) formats
    let ct_bytes = encrypted_note.ct.as_ref();
    if ct_bytes.len() != NOTE_PLAIN_LEN_DEPOSIT && ct_bytes.len() != NOTE_PLAIN_LEN_TRANSFER {
        anyhow::bail!(
            "Invalid ciphertext length: expected {} (deposit) or {} (transfer), got {}",
            NOTE_PLAIN_LEN_DEPOSIT,
            NOTE_PLAIN_LEN_TRANSFER,
            ct_bytes.len()
        );
    }

    // Decrypt into a buffer large enough for either format
    let mut pt = vec![0u8; ct_bytes.len()];
    stream_xor_decrypt(&k, ct_bytes, &mut pt);

    // Parse common fields (present in both formats)
    let mut domain = [0u8; 32];
    domain.copy_from_slice(&pt[0..32]);

    let mut value_bytes = [0u8; 16];
    value_bytes.copy_from_slice(&pt[32..48]);
    let value = u128::from_le_bytes(value_bytes);

    let mut rho = [0u8; 32];
    rho.copy_from_slice(&pt[48..80]);

    let mut recipient = [0u8; 32];
    recipient.copy_from_slice(&pt[80..112]);

    // Parse sender_id if present (144-byte transfer format)
    let sender_id = if pt.len() == NOTE_PLAIN_LEN_TRANSFER {
        let mut sender = [0u8; 32];
        sender.copy_from_slice(&pt[112..144]);
        Some(hex::encode(sender))
    } else {
        None
    };

    Ok(DecryptedNote {
        domain: hex::encode(domain),
        value: value.to_string(),
        rho: hex::encode(rho),
        recipient: hex::encode(recipient),
        sender_id,
    })
}

/// Try to decrypt all encrypted notes using the VFK registry.
///
/// For each note, looks up the correct VFK based on the fvk_commitment.
/// Returns a JSON array of decrypted notes, or None if no notes could be decrypted.
pub fn try_decrypt_notes_with_registry(
    registry: &VfkRegistry,
    encrypted_notes_json: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let json = encrypted_notes_json?;
    let notes: Vec<EncryptedNote> = serde_json::from_value(json.clone()).ok()?;

    if notes.is_empty() {
        return None;
    }

    let mut decrypted = Vec::new();
    for (idx, note) in notes.iter().enumerate() {
        // Look up the VFK by the note's fvk_commitment
        let commitment_hex = hex::encode(note.fvk_commitment);
        if let Some(vfk) = registry.get_vfk(&commitment_hex) {
            match decrypt_note(&vfk, note) {
                Ok(decrypted_note) => {
                    debug!(
                        "Decrypted note {} with VFK commitment {}: value={}",
                        idx,
                        &commitment_hex[..16],
                        decrypted_note.value
                    );
                    decrypted.push(decrypted_note);
                }
                Err(e) => {
                    warn!("Failed to decrypt note {} despite matching VFK: {}", idx, e);
                }
            }
        } else {
            debug!(
                "No VFK found for note {} with commitment {}",
                idx,
                &commitment_hex[..16]
            );
        }
    }

    if decrypted.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&decrypted).ok()?)
    }
}

/// Backward-compatible function: Try to decrypt with a single VFK
#[allow(unused)]
pub fn try_decrypt_notes_json(
    vfk: &Hash32,
    encrypted_notes_json: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let json = encrypted_notes_json?;
    let notes: Vec<EncryptedNote> = serde_json::from_value(json.clone()).ok()?;

    if notes.is_empty() {
        return None;
    }

    let mut decrypted = Vec::new();
    for (idx, note) in notes.iter().enumerate() {
        match decrypt_note(vfk, note) {
            Ok(decrypted_note) => {
                debug!("Decrypted note {}: value={}", idx, decrypted_note.value);
                decrypted.push(decrypted_note);
            }
            Err(e) => {
                debug!(
                    "Failed to decrypt note {} (may not be for this VFK): {}",
                    idx, e
                );
            }
        }
    }

    if decrypted.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&decrypted).ok()?)
    }
}

/// Convert a 32-byte hex string to a bech32m privacy address.
///
/// Input can be with or without `0x` prefix.
/// Returns `privpool1...` format address, or None if input is invalid.
pub fn hex_to_bech32m_address(hex_str: &str) -> Option<String> {
    let s = hex_str.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);

    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }

    let hrp = Hrp::parse(PRIVACY_ADDRESS_HRP).ok()?;
    bech32::encode::<Bech32m>(hrp, &bytes).ok()
}

/// Extract recipient address from decrypted notes as bech32m.
///
/// Looks for the first note with a `recipient` field and converts it to bech32m.
pub fn extract_recipient_from_decrypted_notes(
    decrypted_notes: Option<&serde_json::Value>,
) -> Option<String> {
    let notes = decrypted_notes?;
    let arr = notes.as_array()?;

    for note in arr {
        if let Some(recipient_hex) = note.get("recipient").and_then(|r| r.as_str()) {
            if let Some(bech32_addr) = hex_to_bech32m_address(recipient_hex) {
                return Some(bech32_addr);
            }
        }
    }
    None
}
