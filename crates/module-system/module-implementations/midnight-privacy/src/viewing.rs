//! Viewing key encryption for Midnight Privacy notes - Level B (Poseidon-based).
//!
//! ## Design (Level B - Full Fix)
//!
//! - One shareable FullViewingKey (FVK) (32 bytes).
//! - For a given note commitment `cm`, derive a per-note key via Poseidon2 KDF:
//!     k = H("VIEW_KDF_V1" || fvk || cm)
//! - Encrypt serialized `Note` using deterministic Poseidon2-stream XOR:
//!     ct = pt XOR Stream(k), where Stream produces 32-byte blocks via Poseidon2
//! - Compute ct_hash = H("CT_HASH_V1" || ct) and mac = H("VIEW_MAC_V1" || k || cm || ct_hash)
//! - The guest circuit binds (ct_hash, mac) as public outputs.
//! - The module verifies ct_hash matches the on-chain ciphertext bytes and checks mac.
//!
//! ## Cryptographic choices
//!
//! * Encryption: Poseidon2-stream XOR (SNARK-friendly, deterministic)
//! * KDF: Poseidon2 domain-separated hash to derive (key) from (FVK, cm)
//! * Binding: ct_hash + MAC bind ciphertext to (viewer, note commitment) inside the proof
//!
//! ## Design rationale
//!
//! Level B transforms viewer payloads from "best-effort" into cryptographically enforced
//! attestations. The chain rejects any tx where ciphertext bytes don't match the proof's
//! ct_hash, preventing garbage or mismatched ciphertexts.
//!
//! This follows the "no trust me bro" principle:
//! - Viewers decrypt notes off-chain
//! - Recompute the commitment from plaintext
//! - Verify it matches the on-chain commitment
//! - Chain already rejected non-truthful ciphertexts via Level B checks

use anyhow::{anyhow, Result};

use crate::hash::{note_commitment, poseidon2_hash, Hash32};
use crate::types::{EncryptedNote, FullViewingKey, Note};

/// Compute FVK commitment: H("FVK_COMMIT_V1" || fvk)
pub fn fvk_commitment(fvk: &FullViewingKey) -> Hash32 {
    poseidon2_hash(b"FVK_COMMIT_V1", &[&fvk.0])
}

/// Derive per-note viewing key: H("VIEW_KDF_V1" || fvk || cm)
pub fn view_kdf(fvk: &FullViewingKey, cm: &Hash32) -> Hash32 {
    poseidon2_hash(b"VIEW_KDF_V1", &[&fvk.0, cm])
}

/// Produce the i-th 32-byte stream block for key k using Poseidon2.
fn stream_block(k: &Hash32, ctr: u32) -> Hash32 {
    let c = ctr.to_le_bytes();
    poseidon2_hash(b"VIEW_STREAM_V1", &[k, &c])
}

/// SNARK-friendly deterministic encryption: XOR plaintext with Poseidon-based keystream.
fn stream_xor_encrypt(k: &Hash32, pt: &[u8]) -> Vec<u8> {
    let mut ct = vec![0u8; pt.len()];
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < pt.len() {
        let ks = stream_block(k, ctr);
        ctr = ctr.wrapping_add(1);

        let take = core::cmp::min(32, pt.len() - off);
        for i in 0..take {
            ct[off + i] = pt[off + i] ^ ks[i];
        }
        off += take;
    }
    ct
}

/// SNARK-friendly deterministic decryption: XOR ciphertext with Poseidon-based keystream.
fn stream_xor_decrypt(k: &Hash32, ct: &[u8]) -> Vec<u8> {
    // XOR is symmetric
    stream_xor_encrypt(k, ct)
}

/// Compute ciphertext hash: H("CT_HASH_V1" || ct)
pub fn ct_hash(ct: &[u8]) -> Hash32 {
    poseidon2_hash(b"CT_HASH_V1", &[ct])
}

/// Compute viewing MAC: H("VIEW_MAC_V1" || k || cm || ct_hash)
pub fn view_mac(k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> Hash32 {
    poseidon2_hash(b"VIEW_MAC_V1", &[k, cm, ct_h])
}

/// Deterministic serialization of a Note plaintext (deposit format):
/// [ domain(32) | value_le_16 | rho(32) | recipient(32) ] => 112 bytes
fn encode_note_bytes(note: &Note) -> Vec<u8> {
    let mut pt = Vec::with_capacity(112);
    pt.extend_from_slice(&note.domain);
    pt.extend_from_slice(&note.value.to_le_bytes());
    pt.extend_from_slice(&note.rho);
    pt.extend_from_slice(&note.recipient);
    pt
}

/// Deterministic serialization of a Note plaintext with sender_id (transfer format):
/// [ domain(32) | value_le_16 | rho(32) | recipient(32) | sender_id(32) ] => 144 bytes
fn encode_note_bytes_with_sender(note: &Note, sender_id: &Hash32) -> Vec<u8> {
    let mut pt = Vec::with_capacity(144);
    pt.extend_from_slice(&note.domain);
    pt.extend_from_slice(&note.value.to_le_bytes());
    pt.extend_from_slice(&note.rho);
    pt.extend_from_slice(&note.recipient);
    pt.extend_from_slice(sender_id);
    pt
}

/// Deserialize Note from plaintext.
/// 
/// Supports two formats:
/// - 112 bytes: Deposit notes [domain(32) | value(16) | rho(32) | recipient(32)]
/// - 144 bytes: Spend outputs [domain(32) | value(16) | rho(32) | recipient(32) | sender_id(32)]
/// 
/// For 144-byte format, sender_id is ignored when returning Note (use decode_note_with_sender for full data).
fn decode_note_bytes(pt: &[u8]) -> Result<Note> {
    if pt.len() != 112 && pt.len() != 144 {
        return Err(anyhow!("invalid note plaintext length: {} (expected 112 or 144)", pt.len()));
    }
    let mut domain = [0u8; 32];
    domain.copy_from_slice(&pt[0..32]);
    let mut value_bytes = [0u8; 16];
    value_bytes.copy_from_slice(&pt[32..48]);
    let value = u128::from_le_bytes(value_bytes);
    let mut rho = [0u8; 32];
    rho.copy_from_slice(&pt[48..80]);
    let mut recipient = [0u8; 32];
    recipient.copy_from_slice(&pt[80..112]);
    Ok(Note {
        domain,
        value,
        rho,
        recipient,
    })
}

/// Deserialize Note from plaintext, including optional sender_id.
/// 
/// Returns (Note, Option<sender_id>) where sender_id is present for 144-byte spend outputs.
pub fn decode_note_with_sender(pt: &[u8]) -> Result<(Note, Option<Hash32>)> {
    if pt.len() != 112 && pt.len() != 144 {
        return Err(anyhow!("invalid note plaintext length: {} (expected 112 or 144)", pt.len()));
    }
    let note = decode_note_bytes(pt)?;
    
    let sender_id = if pt.len() == 144 {
        let mut sender = [0u8; 32];
        sender.copy_from_slice(&pt[112..144]);
        Some(sender)
    } else {
        None
    };
    
    Ok((note, sender_id))
}

/// Encrypt a `Note` for a given `cm` using the FVK (Level B - Poseidon-based).
///
/// The `cm` must be the commitment of `note` (caller ensures consistency).
///
/// # Process
///
/// 1. Compute fvk_commitment = H("FVK_COMMIT_V1" || fvk)
/// 2. Derive per-note key k = H("VIEW_KDF_V1" || fvk || cm)
/// 3. Serialize the Note deterministically (112 bytes)
/// 4. Encrypt with Poseidon2-stream XOR
/// 5. Compute ct_hash = H("CT_HASH_V1" || ct)
/// 6. Compute mac = H("VIEW_MAC_V1" || k || cm || ct_hash)
/// 7. Return EncryptedNote containing cm, nonce (dummy), ct, fvk_commitment, mac
///
/// # Security (Level B)
///
/// - The key is derived deterministically from (FVK, cm), unique per note
/// - ct_hash and mac bind the ciphertext to (viewer, cm) inside the proof
/// - The module verifies ct_hash matches on-chain ciphertext bytes
/// - This prevents garbage/mismatched ciphertexts from being accepted
///
/// # Example
///
/// ```ignore
/// let fvk = FullViewingKey([42u8; 32]);
/// let note = Note { domain: [1u8; 32], value: 100, rho: [2u8; 32], recipient: [3u8; 32] };
/// let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
/// let enc = encrypt_note_for_fvk_level_b(&fvk, &note, &cm)?;
/// ```
pub fn encrypt_note_for_fvk_level_b(
    fvk: &FullViewingKey,
    note: &Note,
    cm: &Hash32,
) -> Result<EncryptedNote> {
    let fvk_c = fvk_commitment(fvk);
    let k = view_kdf(fvk, cm);

    // Serialize Note deterministically (112 bytes)
    let pt = encode_note_bytes(note);

    // Encrypt with Poseidon2-stream XOR
    let ct_vec = stream_xor_encrypt(&k, &pt);

    // Compute ct_hash and mac
    let ct_h = ct_hash(&ct_vec);
    let mac = view_mac(&k, cm, &ct_h);

    Ok(EncryptedNote {
        cm: *cm,
        nonce: [0u8; 24], // dummy nonce for Level B (kept for backward compat)
        ct: sov_modules_api::SafeVec::try_from(ct_vec)
            .map_err(|_| anyhow!("ciphertext too large"))?,
        fvk_commitment: fvk_c,
        mac,
    })
}

/// Encrypt a `Note` with sender_id for a given `cm` using the FVK (Level B - transfer format).
///
/// This is used for transfer/withdraw outputs where sender_id is required.
/// The plaintext is 144 bytes: [domain | value | rho | recipient | sender_id]
///
/// # Arguments
/// * `fvk` - The Full Viewing Key
/// * `note` - The note to encrypt
/// * `sender_id` - The sender's address (spender's recipient address)
/// * `cm` - The note commitment
///
/// # Returns
/// EncryptedNote with 144-byte ciphertext containing sender_id
pub fn encrypt_note_for_fvk_with_sender(
    fvk: &FullViewingKey,
    note: &Note,
    sender_id: &Hash32,
    cm: &Hash32,
) -> Result<EncryptedNote> {
    let fvk_c = fvk_commitment(fvk);
    let k = view_kdf(fvk, cm);

    // Serialize Note with sender_id (144 bytes)
    let pt = encode_note_bytes_with_sender(note, sender_id);

    // Encrypt with Poseidon2-stream XOR
    let ct_vec = stream_xor_encrypt(&k, &pt);

    // Compute ct_hash and mac
    let ct_h = ct_hash(&ct_vec);
    let mac = view_mac(&k, cm, &ct_h);

    Ok(EncryptedNote {
        cm: *cm,
        nonce: [0u8; 24], // dummy nonce for Level B
        ct: sov_modules_api::SafeVec::try_from(ct_vec)
            .map_err(|_| anyhow!("ciphertext too large"))?,
        fvk_commitment: fvk_c,
        mac,
    })
}

/// Decrypt and *verify* a note (Level B - Poseidon-based). Returns the plaintext Note only if:
///  - FVK matches fvk_commitment in enc
///  - MAC verification passes
///  - Decryption succeeds
///  - note_commitment(note) == enc.cm
///
/// # Process
///
/// 1. Verify H("FVK_COMMIT_V1" || fvk) == enc.fvk_commitment
/// 2. Derive per-note key k = H("VIEW_KDF_V1" || fvk || enc.cm)
/// 3. Recompute ct_hash = H("CT_HASH_V1" || enc.ct)
/// 4. Verify mac = H("VIEW_MAC_V1" || k || enc.cm || ct_hash)
/// 5. Decrypt with Poseidon2-stream XOR
/// 6. Deserialize the Note from plaintext
/// 7. Recompute commitment from Note fields
/// 8. Verify recomputed commitment matches enc.cm
/// 9. Return Note if valid, error otherwise
///
/// # Security (Level B)
///
/// This is the "not a trust me bro" guarantee:
/// - The viewer cannot be fooled by a malicious ciphertext (chain rejected it via ct_hash check)
/// - The recomputed commitment must match the on-chain commitment
/// - Any mismatch indicates the ciphertext is not truthful and is rejected
///
/// # Example
///
/// ```ignore
/// let fvk = FullViewingKey([42u8; 32]);
/// // ... obtain enc from chain events ...
/// match decrypt_and_verify_note_level_b(&fvk, &enc) {
///     Ok(note) => {
///         // Success: note is truthful, commitment matches on-chain
///         println!("Note value: {}", note.value);
///     }
///     Err(e) => {
///         // Failed: either wrong FVK, corrupted data, or non-truthful ciphertext
///         println!("Failed to decrypt: {}", e);
///     }
/// }
/// ```
pub fn decrypt_and_verify_note_level_b(fvk: &FullViewingKey, enc: &EncryptedNote) -> Result<Note> {
    // 1. Verify FVK matches commitment
    let fvk_c = fvk_commitment(fvk);
    if fvk_c != enc.fvk_commitment {
        return Err(anyhow!("fvk_commitment mismatch: wrong viewer key"));
    }

    // 2. Derive key
    let k = view_kdf(fvk, &enc.cm);

    // 3. Recompute ct_hash
    let ct_h = ct_hash(&enc.ct);

    // 4. Verify MAC
    let mac_expected = view_mac(&k, &enc.cm, &ct_h);
    if mac_expected != enc.mac {
        return Err(anyhow!("mac mismatch: ciphertext may be corrupted or tampered"));
    }

    // 5. Decrypt
    let pt_vec = stream_xor_decrypt(&k, &enc.ct);

    // 6. Deserialize Note
    let note = decode_note_bytes(&pt_vec)?;

    // 7. Recompute commitment
    let cm_recomputed = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

    // 8. Verify commitment matches
    if cm_recomputed != enc.cm {
        return Err(anyhow!("commitment mismatch: not truthful"));
    }

    Ok(note)
}

// === Legacy XChaCha20-Poly1305 functions (kept for backward compatibility) ===
// These are now deprecated in favor of Level B Poseidon-based encryption.

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use sha2::Sha256;

const AEAD_INFO: &[u8] = b"MP_NOTE_AEAD_V1";

/// Derive per-note AEAD key and nonce from FVK and commitment (legacy).
fn derive_key_and_nonce_legacy(fvk: &FullViewingKey, cm: &Hash32) -> (Key, XNonce) {
    let hk = Hkdf::<Sha256>::new(Some(cm), &fvk.0);
    let mut okm = [0u8; 56]; // 32 bytes key + 24 bytes nonce
    hk.expand(AEAD_INFO, &mut okm).expect("HKDF expand");

    let mut key = [0u8; 32];
    key.copy_from_slice(&okm[..32]);
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&okm[32..]);

    (Key::from(key), XNonce::from(nonce))
}

/// Encrypt a `Note` using legacy XChaCha20-Poly1305 (deprecated).
#[deprecated(note = "Use encrypt_note_for_fvk_level_b instead")]
pub fn encrypt_note_for_fvk_legacy(
    fvk: &FullViewingKey,
    note: &Note,
    cm: &Hash32,
) -> Result<EncryptedNote> {
    let (key, nonce) = derive_key_and_nonce_legacy(fvk, cm);
    let cipher = XChaCha20Poly1305::new(&key);

    let pt = bincode::serialize(note).map_err(|e| anyhow!("note serialize: {e}"))?;

    let aad = cm;
    let ct = cipher
        .encrypt(&nonce, Payload { msg: &pt, aad })
        .map_err(|e| anyhow!("encrypt: {e}"))?;

    Ok(EncryptedNote {
        cm: *cm,
        nonce: nonce.into(),
        ct: sov_modules_api::SafeVec::try_from(ct).map_err(|_| anyhow!("ciphertext too large"))?,
        fvk_commitment: fvk_commitment(fvk),
        mac: [0u8; 32], // dummy mac for legacy
    })
}

/// Decrypt and verify a note using legacy XChaCha20-Poly1305 (deprecated).
#[deprecated(note = "Use decrypt_and_verify_note_level_b instead")]
pub fn decrypt_and_verify_note_legacy(fvk: &FullViewingKey, enc: &EncryptedNote) -> Result<Note> {
    let (key, _nonce) = derive_key_and_nonce_legacy(fvk, &enc.cm);
    let cipher = XChaCha20Poly1305::new(&key);

    let pt = cipher
        .decrypt(
            &XNonce::from(enc.nonce),
            Payload {
                msg: &enc.ct,
                aad: &enc.cm,
            },
        )
        .map_err(|e| anyhow!("decrypt: {e}"))?;

    let note: Note = bincode::deserialize(&pt).map_err(|e| anyhow!("note deserialize: {e}"))?;

    let cm_recomputed = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
    if cm_recomputed != enc.cm {
        return Err(anyhow!("commitment mismatch: not truthful"));
    }
    Ok(note)
}

// === Public API aliases (default to Level B) ===

/// Encrypt a note for a viewer (uses Level B by default).
pub fn encrypt_note_for_fvk(
    fvk: &FullViewingKey,
    note: &Note,
    cm: &Hash32,
) -> Result<EncryptedNote> {
    encrypt_note_for_fvk_level_b(fvk, note, cm)
}

/// Decrypt and verify a note (uses Level B by default).
pub fn decrypt_and_verify_note(fvk: &FullViewingKey, enc: &EncryptedNote) -> Result<Note> {
    decrypt_and_verify_note_level_b(fvk, enc)
}

/// Decrypt and verify a note, returning optional sender_id (for spend outputs).
/// 
/// Returns (Note, Option<sender_id>) where sender_id is present for 144-byte spend outputs
/// and absent for 112-byte deposit notes.
pub fn decrypt_and_verify_note_with_sender(fvk: &FullViewingKey, enc: &EncryptedNote) -> Result<(Note, Option<Hash32>)> {
    // 1. Verify FVK matches commitment
    let fvk_c = fvk_commitment(fvk);
    if fvk_c != enc.fvk_commitment {
        return Err(anyhow!("fvk_commitment mismatch: wrong viewer key"));
    }

    // 2. Derive key
    let k = view_kdf(fvk, &enc.cm);

    // 3. Recompute ct_hash
    let ct_h = ct_hash(&enc.ct);

    // 4. Verify MAC
    let mac_expected = view_mac(&k, &enc.cm, &ct_h);
    if mac_expected != enc.mac {
        return Err(anyhow!("mac mismatch: ciphertext may be corrupted or tampered"));
    }

    // 5. Decrypt
    let pt_vec = stream_xor_decrypt(&k, &enc.ct);

    // 6. Deserialize Note with optional sender_id
    let (note, sender_id) = decode_note_with_sender(&pt_vec)?;

    // 7. Recompute commitment
    let cm_recomputed = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

    // 8. Verify commitment matches
    if cm_recomputed != enc.cm {
        return Err(anyhow!("commitment mismatch: not truthful"));
    }

    Ok((note, sender_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encrypt_decrypt_verify_level_b() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let enc = encrypt_note_for_fvk_level_b(&fvk, &note, &cm).unwrap();
        let out = decrypt_and_verify_note_level_b(&fvk, &enc).unwrap();

        assert_eq!(out, note);
    }

    #[test]
    fn wrong_fvk_fails_level_b() {
        let fvk1 = FullViewingKey([7u8; 32]);
        let fvk2 = FullViewingKey([8u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let enc = encrypt_note_for_fvk_level_b(&fvk1, &note, &cm).unwrap();
        let result = decrypt_and_verify_note_level_b(&fvk2, &enc);

        assert!(result.is_err());
    }

    #[test]
    fn corrupted_commitment_fails_level_b() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let mut enc = encrypt_note_for_fvk_level_b(&fvk, &note, &cm).unwrap();

        // Corrupt the commitment
        enc.cm[0] ^= 1;

        let result = decrypt_and_verify_note_level_b(&fvk, &enc);
        assert!(result.is_err());
    }

    #[test]
    fn corrupted_ciphertext_fails_level_b() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let mut enc = encrypt_note_for_fvk_level_b(&fvk, &note, &cm).unwrap();

        // Corrupt the ciphertext - this should fail MAC verification
        enc.ct[0] ^= 1;

        let result = decrypt_and_verify_note_level_b(&fvk, &enc);
        assert!(result.is_err());
    }

    #[test]
    fn different_notes_different_ciphertexts_level_b() {
        let fvk = FullViewingKey([7u8; 32]);
        let note1 = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let note2 = Note {
            domain: [1u8; 32],
            value: 456,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };

        let cm1 = note_commitment(&note1.domain, note1.value, &note1.rho, &note1.recipient);
        let cm2 = note_commitment(&note2.domain, note2.value, &note2.rho, &note2.recipient);

        let enc1 = encrypt_note_for_fvk_level_b(&fvk, &note1, &cm1).unwrap();
        let enc2 = encrypt_note_for_fvk_level_b(&fvk, &note2, &cm2).unwrap();

        // Different notes should produce different ciphertexts
        assert_ne!(enc1.ct, enc2.ct);
        assert_ne!(enc1.cm, enc2.cm);
        assert_ne!(enc1.mac, enc2.mac);
    }

    #[test]
    fn deterministic_encryption_level_b() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        // Encrypt the same note twice
        let enc1 = encrypt_note_for_fvk_level_b(&fvk, &note, &cm).unwrap();
        let enc2 = encrypt_note_for_fvk_level_b(&fvk, &note, &cm).unwrap();

        // Should produce identical ciphertexts (deterministic)
        assert_eq!(enc1.ct, enc2.ct);
        assert_eq!(enc1.cm, enc2.cm);
        assert_eq!(enc1.fvk_commitment, enc2.fvk_commitment);
        assert_eq!(enc1.mac, enc2.mac);
    }
}
