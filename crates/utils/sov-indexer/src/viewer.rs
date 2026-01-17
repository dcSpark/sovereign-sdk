//! FVK Registry and decryption support for the indexer
//!
//! Supports multiple FVKs loaded from a config file. Each address has its own FVK,
//! and the indexer looks up the correct FVK based on the fvk_commitment in each note.
//!
//! Uses DashMap for lock-free concurrent access during indexing.

use crate::index_db as idx;
use anyhow::{Context, Result};
use bech32::{Bech32m, Hrp};
use chrono::Utc;
use dashmap::DashMap;
use midnight_privacy::{
    viewing::{ct_hash, fvk_commitment, view_kdf, view_mac},
    EncryptedNote, FullViewingKey, Hash32,
};
use reqwest::StatusCode as HttpStatusCode;
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

/// A single FVK entry from the config file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FvkEntry {
    /// The FVK as hex string (32 bytes = 64 hex chars)
    pub fvk: String,
    /// The shielded address associated with this FVK (optional)
    #[serde(default)]
    pub shielded_address: Option<String>,
}

/// Config file structure for FVK registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FvkConfig {
    /// List of FVK entries
    pub fvks: Vec<FvkEntry>,
}

/// Registry of FVKs indexed by their commitment for fast concurrent lookup.
/// Uses DashMap for lock-free concurrent access during indexing.
#[derive(Debug)]
pub struct FvkRegistry {
    /// Map from fvk_commitment (hex) -> (fvk bytes, shielded_address)
    by_commitment: DashMap<String, (Hash32, Option<String>)>,
}

#[derive(Clone)]
pub struct FvkServiceClient {
    base_url: String,
    admin_token: String,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct FvkLookupResponse {
    fvk: String,
    fvk_commitment: String,
}

impl FvkServiceClient {
    pub fn from_env() -> Result<Option<Self>> {
        let admin_token = std::env::var("MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let Some(admin_token) = admin_token else {
            return Ok(None);
        };

        let base_url = std::env::var("MIDNIGHT_FVK_SERVICE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());

        Ok(Some(Self {
            base_url,
            admin_token,
            http: reqwest::Client::new(),
        }))
    }

    pub async fn fetch_fvk_by_commitment(
        &self,
        expected_fvk_commitment: &Hash32,
    ) -> Result<Option<Hash32>> {
        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/v1/fvk/{}", hex::encode(expected_fvk_commitment));

        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.admin_token)
            .send()
            .await
            .context("FVK service request failed")?;

        if resp.status() == HttpStatusCode::NOT_FOUND {
            return Ok(None);
        }
        if resp.status() == HttpStatusCode::UNAUTHORIZED {
            anyhow::bail!("FVK service unauthorized (check MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN)");
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("FVK service error {status}: {body}");
        }

        let body: FvkLookupResponse = resp
            .json()
            .await
            .context("Failed to parse FVK service response")?;

        let fvk = parse_fvk_hex(&body.fvk)?;
        let fvk_obj = FullViewingKey(fvk);
        let derived = fvk_commitment(&fvk_obj);

        let expected_hex = body.fvk_commitment.trim().trim_start_matches("0x");
        if expected_hex != hex::encode(derived) {
            anyhow::bail!("FVK service returned fvk_commitment that does not match fvk");
        }
        if &derived != expected_fvk_commitment {
            anyhow::bail!("FVK service returned fvk that does not match requested commitment");
        }

        Ok(Some(fvk))
    }
}

impl FvkRegistry {
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

    /// Number of FVKs in the registry
    pub fn len(&self) -> usize {
        self.by_commitment.len()
    }

    /// Add a FVK to the registry (thread-safe, no &mut needed)
    pub fn add(&self, fvk: Hash32, shielded_address: Option<String>) {
        let fvk_obj = FullViewingKey(fvk);
        let commitment = fvk_commitment(&fvk_obj);
        let commitment_hex = hex::encode(commitment);
        self.by_commitment
            .insert(commitment_hex, (fvk, shielded_address));
    }

    /// Remove a FVK by its commitment (thread-safe)
    pub fn remove(&self, commitment_hex: &str) -> bool {
        self.by_commitment.remove(commitment_hex).is_some()
    }

    /// Look up a FVK by its commitment (hex string) and return a copy
    pub fn get_fvk(&self, commitment_hex: &str) -> Option<Hash32> {
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
                let (k, (fvk, addr)) = r.pair();
                (k.clone(), *fvk, addr.clone())
            })
            .collect()
    }

    /// Load FVKs from a JSON config file
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read FVK config file {:?}: {}", path, e))?;

        let config: FvkConfig = serde_json::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("Failed to parse FVK config file {:?}: {}", path, e))?;

        let registry = Self::new();

        for entry in config.fvks {
            let fvk = parse_fvk_hex(&entry.fvk)?;
            registry.add(fvk, entry.shielded_address);
        }

        info!("Loaded {} FVKs from config file {:?}", registry.len(), path);
        Ok(registry)
    }

    /// Save the registry to the database
    pub async fn save_to_db(&self, db: &DatabaseConnection) -> Result<()> {
        use sea_orm::sea_query::OnConflict;

        for entry in self.by_commitment.iter() {
            let (commitment_hex, (fvk, shielded_address)) = entry.pair();
            let model = idx::fvk_registry::ActiveModel {
                fvk_commitment: Set(commitment_hex.clone()),
                fvk: Set(hex::encode(fvk)),
                shielded_address: Set(shielded_address.clone()),
                created_at: Set(Utc::now()),
            };

            idx::fvk_registry::Entity::insert(model)
                .on_conflict(
                    OnConflict::column(idx::fvk_registry::Column::FvkCommitment)
                        .update_columns([
                            idx::fvk_registry::Column::Fvk,
                            idx::fvk_registry::Column::ShieldedAddress,
                        ])
                        .to_owned(),
                )
                .exec(db)
                .await?;
        }

        info!("Saved {} FVKs to database", self.len());
        Ok(())
    }

    pub async fn save_single_to_db(
        db: &DatabaseConnection,
        fvk: Hash32,
        shielded_address: Option<String>,
    ) -> Result<()> {
        use sea_orm::sea_query::OnConflict;

        let fvk_obj = FullViewingKey(fvk);
        let commitment = fvk_commitment(&fvk_obj);
        let commitment_hex = hex::encode(commitment);

        let model = idx::fvk_registry::ActiveModel {
            fvk_commitment: Set(commitment_hex),
            fvk: Set(hex::encode(fvk)),
            shielded_address: Set(shielded_address),
            created_at: Set(Utc::now()),
        };

        idx::fvk_registry::Entity::insert(model)
            .on_conflict(
                OnConflict::column(idx::fvk_registry::Column::FvkCommitment)
                    .update_columns([
                        idx::fvk_registry::Column::Fvk,
                        idx::fvk_registry::Column::ShieldedAddress,
                    ])
                    .to_owned(),
            )
            .exec(db)
            .await?;

        Ok(())
    }

    /// Load the registry from the database
    pub async fn load_from_db(db: &DatabaseConnection) -> Result<Self> {
        let rows = idx::fvk_registry::Entity::find().all(db).await?;

        let registry = Self::new();

        for row in rows {
            let fvk = parse_fvk_hex(&row.fvk)?;
            // We already have the commitment stored, but we re-add to populate our DashMap
            registry.add(fvk, row.shielded_address);
        }

        if !registry.is_empty() {
            info!("Loaded {} FVKs from database", registry.len());
        }

        Ok(registry)
    }
}

impl Default for FvkRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a FVK from a hex string (with or without 0x prefix)
pub fn parse_fvk_hex(fvk_hex: &str) -> Result<Hash32> {
    let s = fvk_hex.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes =
        hex::decode(s).map_err(|e| anyhow::anyhow!("Invalid FVK hex '{}': {}", fvk_hex, e))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "FVK must be 32 bytes (64 hex chars), got {} bytes",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

pub async fn maybe_fetch_missing_fvks_for_encrypted_notes(
    idx_db: &DatabaseConnection,
    registry: &FvkRegistry,
    encrypted_notes_json: Option<&serde_json::Value>,
    fvk_service: Option<&FvkServiceClient>,
) -> Result<()> {
    let Some(client) = fvk_service else {
        return Ok(());
    };
    let Some(json) = encrypted_notes_json else {
        return Ok(());
    };

    let notes: Vec<EncryptedNote> = match serde_json::from_value(json.clone()) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if notes.is_empty() {
        return Ok(());
    }

    let mut missing = std::collections::HashSet::<Hash32>::new();
    for note in notes.iter() {
        let commitment_hex = hex::encode(note.fvk_commitment);
        if registry.get_fvk(&commitment_hex).is_none() {
            missing.insert(note.fvk_commitment);
        }
    }

    for commitment in missing {
        match client.fetch_fvk_by_commitment(&commitment).await {
            Ok(Some(fvk)) => {
                registry.add(fvk, None);
                if let Err(e) = FvkRegistry::save_single_to_db(idx_db, fvk, None).await {
                    warn!("Failed to persist fetched FVK to index DB: {}", e);
                }
            }
            Ok(None) => {
                warn!(
                    "FVK service did not have commitment 0x{} (cannot decrypt notes for it)",
                    hex::encode(commitment)
                );
            }
            Err(e) => {
                warn!(
                    "Failed to fetch FVK for commitment 0x{}: {}",
                    hex::encode(commitment),
                    e
                );
            }
        }
    }

    Ok(())
}

/// Load FVK config file path from environment variable FVK_CONFIG_FILE
pub fn load_fvk_config_path() -> Option<std::path::PathBuf> {
    std::env::var("FVK_CONFIG_FILE")
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

/// Decrypt an encrypted note using the provided FVK.
///
/// Supports both deposit notes (112 bytes, no sender_id) and transfer notes (144 bytes, with sender_id).
pub fn decrypt_note(fvk: &Hash32, encrypted_note: &EncryptedNote) -> Result<DecryptedNote> {
    let fvk_obj = FullViewingKey(*fvk);
    let expected_fvk_c = fvk_commitment(&fvk_obj);

    // Verify FVK commitment matches
    if encrypted_note.fvk_commitment != expected_fvk_c {
        anyhow::bail!("FVK commitment mismatch: note is not encrypted for this viewing key");
    }

    // Derive decryption key
    let k = view_kdf(&fvk_obj, &encrypted_note.cm);

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

/// Try to decrypt all encrypted notes using the FVK registry.
///
/// For each note, looks up the correct FVK based on the fvk_commitment.
/// Returns a JSON array of decrypted notes, or None if no notes could be decrypted.
pub fn try_decrypt_notes_with_registry(
    registry: &FvkRegistry,
    encrypted_notes_json: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let json = encrypted_notes_json?;
    let notes: Vec<EncryptedNote> = serde_json::from_value(json.clone()).ok()?;

    if notes.is_empty() {
        return None;
    }

    let mut decrypted = Vec::new();
    for (idx, note) in notes.iter().enumerate() {
        // Look up the FVK by the note's fvk_commitment
        let commitment_hex = hex::encode(note.fvk_commitment);
        if let Some(fvk) = registry.get_fvk(&commitment_hex) {
            match decrypt_note(&fvk, note) {
                Ok(decrypted_note) => {
                    debug!(
                        "Decrypted note {} with FVK commitment {}: value={}",
                        idx,
                        &commitment_hex[..16],
                        decrypted_note.value
                    );
                    decrypted.push(decrypted_note);
                }
                Err(e) => {
                    warn!("Failed to decrypt note {} despite matching FVK: {}", idx, e);
                }
            }
        } else {
            debug!(
                "No FVK found for note {} with commitment {}",
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

/// Backward-compatible function: Try to decrypt with a single FVK
#[allow(unused)]
pub fn try_decrypt_notes_json(
    fvk: &Hash32,
    encrypted_notes_json: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let json = encrypted_notes_json?;
    let notes: Vec<EncryptedNote> = serde_json::from_value(json.clone()).ok()?;

    if notes.is_empty() {
        return None;
    }

    let mut decrypted = Vec::new();
    for (idx, note) in notes.iter().enumerate() {
        match decrypt_note(fvk, note) {
            Ok(decrypted_note) => {
                debug!("Decrypted note {}: value={}", idx, decrypted_note.value);
                decrypted.push(decrypted_note);
            }
            Err(e) => {
                debug!(
                    "Failed to decrypt note {} (may not be for this FVK): {}",
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

/// Extract sender address from decrypted notes as bech32m.
///
/// Looks for the first note with a `sender_id` field and converts it to bech32m.
pub fn extract_sender_from_decrypted_notes(
    decrypted_notes: Option<&serde_json::Value>,
) -> Option<String> {
    let notes = decrypted_notes?;
    let arr = notes.as_array()?;

    for note in arr {
        if let Some(sender_hex) = note.get("sender_id").and_then(|s| s.as_str()) {
            if let Some(bech32_addr) = hex_to_bech32m_address(sender_hex) {
                return Some(bech32_addr);
            }
        }
    }

    None
}
