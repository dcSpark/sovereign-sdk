//! Level-B Viewer Support (Authority Viewing Key)
//!
//! This module provides helpers for generating viewer attestations and encrypted notes
//! that allow authorities to decrypt shielded transaction data.

use midnight_privacy::{
    viewing::{ct_hash, fvk_commitment, view_kdf, view_mac},
    EncryptedNote, FullViewingKey, Hash32, ViewAttestation,
};

/// Length of note plaintext for deposits: 32(domain) + 16(value) + 32(rho) + 32(recipient)
pub const NOTE_PLAIN_LEN_DEPOSIT: usize = 112;

/// Length of note plaintext for transfers: 32(domain) + 16(value) + 32(rho) + 32(recipient) + 32(sender_id)
pub const NOTE_PLAIN_LEN_TRANSFER: usize = 144;

/// Load authority viewing key from environment variable AUTHORITY_VFK.
///
/// Accepts hex strings with or without `0x` prefix.
/// Returns `None` if:
/// - Environment variable is not set
/// - Hex decoding fails
/// - Length is not exactly 32 bytes
pub fn load_authority_vfk() -> Option<Hash32> {
    let raw = std::env::var("AUTHORITY_VFK").ok()?;
    let s = raw.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = match hex::decode(s) {
        Ok(b) => b,
        Err(_) => return None,
    };
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Produce the i-th 32-byte stream block for key k using Poseidon2.
fn stream_block(k: &Hash32) -> impl Fn(u32) -> Hash32 + '_ {
    move |ctr: u32| {
        let c = ctr.to_le_bytes();
        midnight_privacy::poseidon2_hash(b"VIEW_STREAM_V1", &[k, &c])
    }
}

/// SNARK-friendly deterministic encryption: XOR plaintext with Poseidon-based keystream.
fn stream_xor_encrypt(k: &Hash32, pt: &[u8], ct_out: &mut [u8]) {
    debug_assert_eq!(pt.len(), ct_out.len());
    let block_fn = stream_block(k);
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < pt.len() {
        let ks = block_fn(ctr);
        ctr = ctr.wrapping_add(1);
        let take = core::cmp::min(32, pt.len() - off);
        for i in 0..take {
            ct_out[off + i] = pt[off + i] ^ ks[i];
        }
        off += take;
    }
}

/// Serialize note plaintext for encryption (144 bytes with sender_id).
pub fn encode_note_plain(
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> [u8; NOTE_PLAIN_LEN_TRANSFER] {
    let mut out = [0u8; NOTE_PLAIN_LEN_TRANSFER];
    out[0..32].copy_from_slice(domain);
    // Encode as 16-byte LE, zero-extended from u64.
    out[32..40].copy_from_slice(&value.to_le_bytes());
    out[40..48].copy_from_slice(&[0u8; 8]);
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
    out
}

/// Build both the attestation (for proof) and the EncryptedNote (for tx).
///
/// # Arguments
/// * `vfk` - The Full Viewing Key (32-byte secret)
/// * `domain` - The note domain
/// * `value` - The token amount
/// * `rho` - The note randomness
/// * `recipient` - The recipient identifier
/// * `sender_id` - The sender identifier (spender's address for transfers)
/// * `cm` - The note commitment
///
/// # Returns
/// A tuple of (ViewAttestation, EncryptedNote) where:
/// - ViewAttestation is included in the ZK proof's public output
/// - EncryptedNote is attached to the transaction for authority decryption
pub fn make_viewer_bundle(
    vfk: &Hash32,
    domain: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
    cm: &Hash32,
) -> anyhow::Result<(ViewAttestation, EncryptedNote)> {
    let value_u64: u64 = value
        .try_into()
        .map_err(|_| anyhow::anyhow!("note value does not fit into u64 (required by note_spend_guest v2)"))?;
    let vfk_obj = FullViewingKey(*vfk);
    let vfk_c = fvk_commitment(&vfk_obj);
    let pt = encode_note_plain(domain, value_u64, rho, recipient, sender_id);
    let k = view_kdf(&vfk_obj, cm);
    let mut ct = [0u8; NOTE_PLAIN_LEN_TRANSFER];
    stream_xor_encrypt(&k, &pt, &mut ct);
    let ct_h = ct_hash(&ct);
    let mac = view_mac(&k, cm, &ct_h);

    let enc = EncryptedNote {
        cm: *cm,
        nonce: [0u8; 24],
        ct: sov_modules_api::SafeVec::try_from(ct.to_vec()).expect("ciphertext within limit"),
        fvk_commitment: vfk_c,
        mac,
    };

    let att = ViewAttestation {
        cm: *cm,
        fvk_commitment: vfk_c,
        ct_hash: ct_h,
        mac,
    };

    Ok((att, enc))
}

/// Decrypt an encrypted note using the authority VFK.
///
/// Supports both deposit notes (112 bytes, no sender_id) and transfer notes (144 bytes, with sender_id).
///
/// # Arguments
/// * `vfk` - The Full Viewing Key (32-byte secret)
/// * `encrypted_note` - The encrypted note from the transaction
///
/// # Returns
/// Decrypted note data as (domain, value, rho, recipient, sender_id) where sender_id is Option<Hash32>
/// - For deposits (112 bytes): sender_id is None
/// - For transfers (144 bytes): sender_id is Some(Hash32)
pub fn decrypt_note(
    vfk: &Hash32,
    encrypted_note: &EncryptedNote,
) -> anyhow::Result<(Hash32, u128, Hash32, Hash32, Option<Hash32>)> {
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
    stream_xor_encrypt(&k, ct_bytes, &mut pt);

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
        Some(sender)
    } else {
        None
    };

    Ok((domain, value, rho, recipient, sender_id))
}
