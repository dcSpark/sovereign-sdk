//! Viewing key encryption for Midnight Privacy notes.
//!
//! Design:
//! - One shareable FullViewingKey(FVK) (32 bytes).
//! - For a given note commitment `cm`, derive a per-note AEAD key + nonce via HKDF:
//!     key, nonce = HKDF-SHA256(ikm = FVK, salt = cm, info = "MP_NOTE_AEAD_V1")
//! - Encrypt serialized `Note` using XChaCha20-Poly1305 with AAD = cm.
//! - Viewer decrypts, deserializes Note, recomputes commitment, and verifies cm' == cm.
//!
//! This prevents "trust me bro": decryption alone is not enough; the viewer must
//! recompute the *on-chain* commitment from the recovered plaintext.
//!
//! # Cryptographic choices
//!
//! * AEAD: XChaCha20-Poly1305 (nonce misuse resistant, 24-byte nonce, fast)
//! * KDF: HKDF-SHA256 to derive (aead_key, nonce) from (FVK, cm)
//! * AAD: the 32-byte cm so ciphertext is bound to the specific on-chain note
//!
//! # Design rationale
//!
//! This follows Zcash's viewing key pattern (ZIP 32/316):
//! - Viewers decrypt notes
//! - Recompute the commitment from plaintext
//! - Verify it matches the on-chain commitment
//! - Reject any mismatches
//!
//! This ensures the ciphertext is "truthful" - it cannot lie about the note contents
//! because the commitment binds the plaintext to the on-chain state.

use anyhow::{anyhow, Result};
use chacha20poly1305::{aead::{Aead, KeyInit, Payload}, XChaCha20Poly1305, Key, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::hash::{note_commitment, Hash32};
use crate::types::{EncryptedNote, FullViewingKey, Note};

const AEAD_INFO: &[u8] = b"MP_NOTE_AEAD_V1";

/// Derive per-note AEAD key and nonce from FVK and commitment.
///
/// Uses HKDF-SHA256 with:
/// - IKM (Input Keying Material): FVK (32 bytes)
/// - Salt: cm (32 bytes, the note commitment)
/// - Info: "MP_NOTE_AEAD_V1" (domain separation)
///
/// Output: 56 bytes total (32-byte key + 24-byte nonce for XChaCha20-Poly1305)
///
/// This derivation is deterministic: the same FVK and cm always produce the same key/nonce.
/// This allows the viewer to decrypt without requiring ephemeral keys or per-note randomness.
fn derive_key_and_nonce(fvk: &FullViewingKey, cm: &Hash32) -> (Key, XNonce) {
    // HKDF-Extract(salt = cm, ikm = fvk)
    let hk = Hkdf::<Sha256>::new(Some(cm), &fvk.0);
    let mut okm = [0u8; 56]; // 32 bytes key + 24 bytes nonce
    hk.expand(AEAD_INFO, &mut okm).expect("HKDF expand");

    let mut key = [0u8; 32];
    key.copy_from_slice(&okm[..32]);
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&okm[32..]);

    (Key::from(key), XNonce::from(nonce))
}

/// Encrypt a `Note` for a given `cm` using the FVK.
///
/// The `cm` must be the commitment of `note` (caller ensures consistency).
///
/// # Process
///
/// 1. Derive per-note AEAD key and nonce from (FVK, cm)
/// 2. Serialize the Note using bincode (compact, deterministic)
/// 3. Encrypt with XChaCha20-Poly1305, binding to cm via AAD
/// 4. Return EncryptedNote containing cm, nonce, and ciphertext
///
/// # Security
///
/// - The nonce is derived deterministically from (FVK, cm), so it's unique per note
/// - The AAD includes cm, binding the ciphertext to the specific commitment
/// - XChaCha20-Poly1305 provides authenticated encryption (confidentiality + integrity)
///
/// # Example
///
/// ```ignore
/// let fvk = FullViewingKey([42u8; 32]);
/// let note = Note { domain: [1u8; 32], value: 100, rho: [2u8; 32], recipient: [3u8; 32] };
/// let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
/// let enc = encrypt_note_for_fvk(&fvk, &note, &cm)?;
/// ```
pub fn encrypt_note_for_fvk(fvk: &FullViewingKey, note: &Note, cm: &Hash32) -> Result<EncryptedNote> {
    let (key, nonce) = derive_key_and_nonce(fvk, cm);
    let cipher = XChaCha20Poly1305::new(&key);

    // Serialize Note with bincode (compact, deterministic)
    let pt = bincode::serialize(note)
        .map_err(|e| anyhow!("note serialize: {e}"))?;

    // Bind ciphertext to cm via AAD = cm
    let aad = cm;
    let ct = cipher.encrypt(&nonce, Payload { msg: &pt, aad })
        .map_err(|e| anyhow!("encrypt: {e}"))?;

    Ok(EncryptedNote {
        cm: *cm,
        nonce: nonce.into(),
        ct: sov_modules_api::SafeVec::try_from(ct)
            .map_err(|_| anyhow!("ciphertext too large"))?,
    })
}

/// Decrypt and *verify* a note. Returns the plaintext Note only if:
///  - AEAD decrypts successfully, and
///  - note_commitment(note) == enc.cm
///
/// # Process
///
/// 1. Derive per-note AEAD key and nonce from (FVK, enc.cm)
/// 2. Decrypt the ciphertext with XChaCha20-Poly1305 (verifies AAD = enc.cm)
/// 3. Deserialize the Note from plaintext
/// 4. Recompute commitment from Note fields
/// 5. Verify recomputed commitment matches enc.cm
/// 6. Return Note if valid, error otherwise
///
/// # Security
///
/// This is the "not a trust me bro" guarantee:
/// - The viewer cannot be fooled by a malicious ciphertext
/// - The recomputed commitment must match the on-chain commitment
/// - Any mismatch indicates the ciphertext is not truthful and is rejected
///
/// This follows Zcash's pattern: recipients verify that decrypted notes match
/// the on-chain commitments before accepting them.
///
/// # Example
///
/// ```ignore
/// let fvk = FullViewingKey([42u8; 32]);
/// // ... obtain enc from chain events ...
/// match decrypt_and_verify_note(&fvk, &enc) {
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
pub fn decrypt_and_verify_note(fvk: &FullViewingKey, enc: &EncryptedNote) -> Result<Note> {
    let (key, _nonce) = derive_key_and_nonce(fvk, &enc.cm);
    let cipher = XChaCha20Poly1305::new(&key);

    let pt = cipher.decrypt(
        &XNonce::from(enc.nonce),
        Payload { msg: &enc.ct, aad: &enc.cm }
    ).map_err(|e| anyhow!("decrypt: {e}"))?;

    let note: Note = bincode::deserialize(&pt)
        .map_err(|e| anyhow!("note deserialize: {e}"))?;

    // "Not a trust me bro": recompute and compare to the on-chain cm
    let cm_recomputed = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
    if cm_recomputed != enc.cm {
        return Err(anyhow!("commitment mismatch: not truthful"));
    }
    Ok(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encrypt_decrypt_verify() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let enc = encrypt_note_for_fvk(&fvk, &note, &cm).unwrap();
        let out = decrypt_and_verify_note(&fvk, &enc).unwrap();

        assert_eq!(out, note);
    }

    #[test]
    fn wrong_fvk_fails() {
        let fvk1 = FullViewingKey([7u8; 32]);
        let fvk2 = FullViewingKey([8u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let enc = encrypt_note_for_fvk(&fvk1, &note, &cm).unwrap();
        let result = decrypt_and_verify_note(&fvk2, &enc);

        assert!(result.is_err());
    }

    #[test]
    fn corrupted_commitment_fails() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let mut enc = encrypt_note_for_fvk(&fvk, &note, &cm).unwrap();
        
        // Corrupt the commitment
        enc.cm[0] ^= 1;
        
        let result = decrypt_and_verify_note(&fvk, &enc);
        assert!(result.is_err());
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        let mut enc = encrypt_note_for_fvk(&fvk, &note, &cm).unwrap();
        
        // Corrupt the ciphertext
        enc.ct[0] ^= 1;
        
        let result = decrypt_and_verify_note(&fvk, &enc);
        assert!(result.is_err());
    }

    #[test]
    fn different_notes_different_ciphertexts() {
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

        let enc1 = encrypt_note_for_fvk(&fvk, &note1, &cm1).unwrap();
        let enc2 = encrypt_note_for_fvk(&fvk, &note2, &cm2).unwrap();

        // Different notes should produce different ciphertexts
        assert_ne!(enc1.ct, enc2.ct);
        assert_ne!(enc1.cm, enc2.cm);
    }

    #[test]
    fn deterministic_encryption() {
        let fvk = FullViewingKey([7u8; 32]);
        let note = Note {
            domain: [1u8; 32],
            value: 123,
            rho: [2u8; 32],
            recipient: [3u8; 32],
        };
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);

        // Encrypt the same note twice
        let enc1 = encrypt_note_for_fvk(&fvk, &note, &cm).unwrap();
        let enc2 = encrypt_note_for_fvk(&fvk, &note, &cm).unwrap();

        // Should produce identical ciphertexts (deterministic)
        assert_eq!(enc1.nonce, enc2.nonce);
        assert_eq!(enc1.ct, enc2.ct);
        assert_eq!(enc1.cm, enc2.cm);
    }
}

