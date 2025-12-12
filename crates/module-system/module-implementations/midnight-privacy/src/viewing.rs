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
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use crate::hash::{clamp_x25519_scalar, note_commitment, poseidon2_hash, recipient_from_pk, Hash32};
use crate::types::{EncryptedNote, FullViewingKey, Note, RecipientCiphertext};

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

// =============================================================================
// Incoming encryption (for recipient detection)
// =============================================================================
//
// These functions implement X25519-based encryption for notes, allowing the
// recipient to detect and decrypt notes sent to them. This is separate from
// viewer encryption (which uses FVK for auditors).
//
// Key derivation:
// - ivk_sk = H("IVK_SEED_V1" || domain || spend_sk)
// - pk_ivk = X25519_BASE(clamp(ivk_sk))
// - esk = clamp(H("IVK_SEED_V1" || domain || rho))  // ephemeral secret from rho
// - epk = X25519_BASE(esk)
// - dh = X25519(esk, pk_ivk) = X25519(ivk_sk, epk)
// - k_in = H("IN_KDF_V1" || domain || dh || cm)
//
// Encryption uses IN_STREAM_V1 for domain separation from viewer streams.

/// Derive ephemeral secret key from (domain, rho, cm).
/// esk = clamp(H("ESK_V2" || domain || rho || cm))
///
/// SECURITY: Including cm in the derivation provides defense-in-depth:
/// - If rho is accidentally reused across different notes, cm will differ,
///   so esk (and thus epk) will still be unique.
/// - This reduces public linkability: without cm, reusing rho would cause
///   epk to repeat publicly, linking outputs even if commitments differ.
pub fn esk_from_rho_cm(domain: &Hash32, rho: &Hash32, cm: &Hash32) -> [u8; 32] {
    let seed = poseidon2_hash(b"ESK_V2", &[domain, rho, cm]);
    clamp_x25519_scalar(seed)
}

/// Compute ephemeral public key: epk = X25519_BASE(esk)
pub fn epk_from_rho_cm(domain: &Hash32, rho: &Hash32, cm: &Hash32) -> Hash32 {
    let esk = esk_from_rho_cm(domain, rho, cm);
    let secret = X25519Secret::from(esk);
    let public = X25519PublicKey::from(&secret);
    *public.as_bytes()
}

/// Compute X25519 shared secret
fn x25519_shared(sk_bytes: &[u8; 32], pk_bytes: &[u8; 32]) -> Hash32 {
    let secret = X25519Secret::from(*sk_bytes);
    let public = X25519PublicKey::from(*pk_bytes);
    *secret.diffie_hellman(&public).as_bytes()
}

/// Derive incoming encryption key: k_in = H("IN_KDF_V1" || domain || dh || cm)
pub fn in_kdf(domain: &Hash32, dh: &Hash32, cm: &Hash32) -> Hash32 {
    poseidon2_hash(b"IN_KDF_V1", &[domain, dh, cm])
}

/// Compute incoming MAC: H("IN_MAC_V1" || k || cm || ct_hash)
pub fn in_mac(k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> Hash32 {
    poseidon2_hash(b"IN_MAC_V1", &[k, cm, ct_h])
}

/// Produce the i-th 32-byte stream block for incoming encryption.
fn in_stream_block(k: &Hash32, ctr: u32) -> Hash32 {
    let c = ctr.to_le_bytes();
    poseidon2_hash(b"IN_STREAM_V1", &[k, &c])
}

/// XOR encrypt/decrypt with IN_STREAM_V1 keystream
fn stream_xor_in(k: &Hash32, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; data.len()];
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < data.len() {
        let ks = in_stream_block(k, ctr);
        ctr = ctr.wrapping_add(1);
        let take = core::cmp::min(32, data.len() - off);
        for i in 0..take {
            out[off + i] = data[off + i] ^ ks[i];
        }
        off += take;
    }
    out
}

/// Encrypt a note for the recipient using X25519 key agreement.
///
/// This creates a `RecipientCiphertext` that allows the receiver to detect
/// and decrypt the note using their `ivk_sk`.
///
/// # Parameters
/// - `domain`: Chain domain tag
/// - `pk_ivk`: Recipient's incoming viewing public key (X25519)
/// - `note`: The note to encrypt
/// - `cm`: The note commitment (must match the note)
///
/// # Returns
/// `RecipientCiphertext` containing `(cm, epk, ct, mac)`
///
/// # Process
/// 1. Derive ephemeral secret: esk = esk_from_rho_cm(domain, rho, cm)
/// 2. Compute ephemeral public key: epk = X25519_BASE(esk)
/// 3. Compute DH shared secret: dh = X25519(esk, pk_ivk)
/// 4. Derive symmetric key: k_in = H("IN_KDF_V1" || domain || dh || cm)
/// 5. Serialize note (112 bytes)
/// 6. Encrypt: ct = pt XOR IN_STREAM(k_in)
/// 7. Compute ct_hash = H("CT_HASH_V1" || ct)
/// 8. Compute mac = H("IN_MAC_V1" || k_in || cm || ct_hash)
pub fn encrypt_note_for_recipient(
    domain: &Hash32,
    pk_ivk: &Hash32,
    note: &Note,
    cm: &Hash32,
) -> Result<RecipientCiphertext> {
    // 1-2. Derive ephemeral keys from (domain, rho, cm) - cm binding reduces linkability
    let esk = esk_from_rho_cm(domain, &note.rho, cm);
    let epk = epk_from_rho_cm(domain, &note.rho, cm);
    
    // 3. DH shared secret
    let dh = x25519_shared(&esk, pk_ivk);
    
    // Check for low-order points (all-zero DH output)
    if dh == [0u8; 32] {
        return Err(anyhow!("X25519 produced all-zero output (low-order point)"));
    }
    
    // 4. Derive symmetric key
    let k_in = in_kdf(domain, &dh, cm);
    
    // 5. Serialize note (112 bytes: domain | value | rho | recipient)
    let pt = encode_note_bytes(note);
    
    // 6. Encrypt with IN_STREAM_V1
    let ct = stream_xor_in(&k_in, &pt);
    
    // 7-8. Compute ct_hash and MAC
    let ct_h = ct_hash(&ct);
    let mac = in_mac(&k_in, cm, &ct_h);
    
    Ok(RecipientCiphertext {
        cm: *cm,
        epk,
        ct,
        mac,
    })
}

/// Encrypt a note with sender_id for the recipient (transfer format).
///
/// Same as `encrypt_note_for_recipient` but includes the 32-byte sender_id
/// in the plaintext (144 bytes total).
pub fn encrypt_note_for_recipient_with_sender(
    domain: &Hash32,
    pk_ivk: &Hash32,
    note: &Note,
    sender_id: &Hash32,
    cm: &Hash32,
) -> Result<RecipientCiphertext> {
    // Derive ephemeral keys from (domain, rho, cm) - cm binding reduces linkability
    let esk = esk_from_rho_cm(domain, &note.rho, cm);
    let epk = epk_from_rho_cm(domain, &note.rho, cm);
    let dh = x25519_shared(&esk, pk_ivk);
    
    if dh == [0u8; 32] {
        return Err(anyhow!("X25519 produced all-zero output (low-order point)"));
    }
    
    let k_in = in_kdf(domain, &dh, cm);
    let pt = encode_note_bytes_with_sender(note, sender_id);
    let ct = stream_xor_in(&k_in, &pt);
    let ct_h = ct_hash(&ct);
    let mac = in_mac(&k_in, cm, &ct_h);
    
    Ok(RecipientCiphertext {
        cm: *cm,
        epk,
        ct,
        mac,
    })
}

/// Attempt to decrypt and verify a recipient ciphertext.
///
/// This is used by receivers scanning the chain for notes sent to them.
///
/// # Parameters
/// - `domain`: Chain domain tag
/// - `ivk_sk`: Receiver's incoming viewing secret key (derived from spend_sk)
/// - `my_pk_spend`: Receiver's spending public key (for ownership verification)
/// - `my_pk_ivk`: Receiver's incoming viewing public key (for address derivation)
/// - `enc`: The recipient ciphertext from the chain
///
/// # Returns
/// - `Ok(Note)` if decryption succeeds, MAC verifies, and the note belongs to us
/// - `Err` if any verification step fails
///
/// # Process
/// 1. Compute dh = X25519(ivk_sk, epk)
/// 2. If dh is all-zero, fail (low-order point)
/// 3. Derive k_in = H("IN_KDF_V1" || domain || dh || cm)
/// 4. Decrypt pt = ct XOR IN_STREAM(k_in)
/// 5. Verify MAC = H("IN_MAC_V1" || k_in || cm || H("CT_HASH_V1" || ct))
/// 6. Parse note from plaintext
/// 7. Verify cm = note_commitment(note fields)
/// 8. **CRITICAL**: Verify recipient_in_note == H("ADDR_V2" || domain || my_pk_spend || my_pk_ivk)
///    This rejects spam/deceptive notes that are decryptable but not spendable by us.
///
/// # Security Note
/// With ADDR_V2, both pk_spend and pk_ivk are bound into the address, so mismatched
/// encryption attacks are prevented at the protocol level (note commitment wouldn't match).
/// This check now validates that the note was truly sent to our complete address.
pub fn decrypt_and_verify_recipient_note(
    domain: &Hash32,
    ivk_sk: &Hash32,
    my_pk_spend: &Hash32,
    my_pk_ivk: &Hash32,
    enc: &RecipientCiphertext,
) -> Result<Note> {
    // 1. Compute DH shared secret
    let clamped = clamp_x25519_scalar(*ivk_sk);
    let dh = x25519_shared(&clamped, &enc.epk);
    
    // 2. Check for low-order points
    if dh == [0u8; 32] {
        return Err(anyhow!("X25519 produced all-zero output (low-order point)"));
    }
    
    // 3. Derive symmetric key
    let k_in = in_kdf(domain, &dh, &enc.cm);
    
    // 4. Decrypt
    let pt = stream_xor_in(&k_in, &enc.ct);
    
    // 5. Verify MAC
    let ct_h = ct_hash(&enc.ct);
    let expected_mac = in_mac(&k_in, &enc.cm, &ct_h);
    if expected_mac != enc.mac {
        return Err(anyhow!("MAC verification failed"));
    }
    
    // 6. Parse note
    let note = decode_note_bytes(&pt)?;
    
    // 7. Verify commitment
    let cm_computed = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
    if cm_computed != enc.cm {
        return Err(anyhow!(
            "commitment mismatch: computed {} != stored {}",
            hex::encode(cm_computed),
            hex::encode(enc.cm)
        ));
    }
    
    // 8. CRITICAL: Verify this note is actually ours (spendable)
    // With ADDR_V2, both pk_spend and pk_ivk are bound into the address
    let my_recipient = recipient_from_pk(domain, my_pk_spend, my_pk_ivk);
    if note.recipient != my_recipient {
        return Err(anyhow!(
            "note recipient does not match our address - this note was not sent to us"
        ));
    }
    
    Ok(note)
}

/// Attempt to decrypt a recipient ciphertext without ownership verification.
///
/// This is a lower-level function that skips the critical step 8 (recipient verification).
/// Use `decrypt_and_verify_recipient_note` in production wallets.
///
/// # Returns
/// - `Ok((Note, Option<sender_id>))` if decryption and MAC verification succeed
/// - `Err` if decryption fails
///
/// # Warning
/// This does NOT verify the note is spendable by you. Use `decrypt_and_verify_recipient_note`
/// for wallet scanning to reject spam/deceptive notes.
pub fn decrypt_recipient_note_unchecked(
    domain: &Hash32,
    ivk_sk: &Hash32,
    enc: &RecipientCiphertext,
) -> Result<(Note, Option<Hash32>)> {
    let clamped = clamp_x25519_scalar(*ivk_sk);
    let dh = x25519_shared(&clamped, &enc.epk);
    
    if dh == [0u8; 32] {
        return Err(anyhow!("X25519 produced all-zero output"));
    }
    
    let k_in = in_kdf(domain, &dh, &enc.cm);
    let pt = stream_xor_in(&k_in, &enc.ct);
    
    let ct_h = ct_hash(&enc.ct);
    let expected_mac = in_mac(&k_in, &enc.cm, &ct_h);
    if expected_mac != enc.mac {
        return Err(anyhow!("MAC verification failed"));
    }
    
    let (note, sender_id) = decode_note_with_sender(&pt)?;
    
    let cm_computed = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
    if cm_computed != enc.cm {
        return Err(anyhow!("commitment mismatch"));
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
