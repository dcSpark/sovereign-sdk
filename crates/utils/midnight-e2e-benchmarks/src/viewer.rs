//! Level-B Viewer Support (Authority Viewing Key)
//!
//! This module provides helpers for generating viewer attestations and encrypted notes
//! that allow authorities to decrypt shielded transaction data.

use midnight_privacy::{
    viewing::{ct_hash, fvk_commitment, view_kdf, view_mac},
    EncryptedNote, FullViewingKey, Hash32, ViewAttestation,
};

/// Length of note plaintext: 32(domain) + 16(value) + 32(rho) + 32(recipient) + 32(sender_id)
pub const NOTE_PLAIN_LEN: usize = 144;

/// Load authority viewing key from environment variable AUTHORITY_FVK.
///
/// Accepts hex strings with or without `0x` prefix.
/// Returns `None` if:
/// - Environment variable is not set
/// - Hex decoding fails
/// - Length is not exactly 32 bytes
pub fn load_authority_fvk() -> Option<Hash32> {
    let raw = std::env::var("AUTHORITY_FVK").ok()?;
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
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> [u8; NOTE_PLAIN_LEN] {
    let mut out = [0u8; NOTE_PLAIN_LEN];
    out[0..32].copy_from_slice(domain);
    out[32..48].copy_from_slice(&value.to_le_bytes());
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
    out
}

/// Build both the attestation (for proof) and the EncryptedNote (for tx).
///
/// # Arguments
/// * `fvk` - The Full Viewing Key (32-byte secret)
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
    fvk: &Hash32,
    domain: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
    cm: &Hash32,
) -> (ViewAttestation, EncryptedNote) {
    let fvk_obj = FullViewingKey(*fvk);
    let fvk_c = fvk_commitment(&fvk_obj);
    let pt = encode_note_plain(domain, value, rho, recipient, sender_id);
    let k = view_kdf(&fvk_obj, cm);
    let mut ct = [0u8; NOTE_PLAIN_LEN];
    stream_xor_encrypt(&k, &pt, &mut ct);
    let ct_h = ct_hash(&ct);
    let mac = view_mac(&k, cm, &ct_h);

    let enc = EncryptedNote {
        cm: *cm,
        nonce: [0u8; 24],
        ct: sov_modules_api::SafeVec::try_from(ct.to_vec()).expect("ciphertext within limit"),
        fvk_commitment: fvk_c,
        mac,
    };

    let att = ViewAttestation {
        cm: *cm,
        fvk_commitment: fvk_c,
        ct_hash: ct_h,
        mac,
    };

    (att, enc)
}
