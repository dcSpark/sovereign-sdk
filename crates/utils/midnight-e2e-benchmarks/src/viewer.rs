//! Level-B Viewer Support (Viewer Full Viewing Key)
//!
//! This module provides helpers for generating viewer attestations and encrypted notes
//! that allow configured viewers to decrypt shielded transaction data.

use midnight_privacy::{
    viewing::{ct_hash, fvk_commitment, view_kdf, view_mac},
    EncryptedNote, FullViewingKey, Hash32, ViewAttestation,
};

/// Legacy spend/output note plaintext length (no `cm_ins`).
pub const NOTE_PLAIN_LEN_SPEND_V1: usize = 144;

pub const MAX_INS: usize = 4;

/// Current spend/output note plaintext length (includes `cm_ins[4]`).
pub const NOTE_PLAIN_LEN_SPEND_V2: usize = NOTE_PLAIN_LEN_SPEND_V1 + 32 * MAX_INS;

// Backward-compatible alias (historically 144, now reflects current spend/output plaintext size).
pub const NOTE_PLAIN_LEN: usize = NOTE_PLAIN_LEN_SPEND_V2;

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

/// Serialize spend/output note plaintext for encryption (includes `cm_ins`).
pub fn encode_note_plain(
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
    cm_ins: &[Hash32; MAX_INS],
) -> [u8; NOTE_PLAIN_LEN_SPEND_V2] {
    let mut out = [0u8; NOTE_PLAIN_LEN_SPEND_V2];
    out[0..32].copy_from_slice(domain);
    // Encode as 16-byte LE, zero-extended from u64.
    out[32..40].copy_from_slice(&value.to_le_bytes());
    out[40..48].copy_from_slice(&[0u8; 8]);
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
    let mut off = 144usize;
    for cm in cm_ins {
        out[off..off + 32].copy_from_slice(cm);
        off += 32;
    }
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
    cm_ins: &[Hash32; MAX_INS],
    cm: &Hash32,
) -> anyhow::Result<(ViewAttestation, EncryptedNote)> {
    let value_u64: u64 = value.try_into().map_err(|_| {
        anyhow::anyhow!("note value does not fit into u64 (required by note_spend_guest v2)")
    })?;
    let fvk_obj = FullViewingKey(*fvk);
    let fvk_c = fvk_commitment(&fvk_obj);
    let pt = encode_note_plain(domain, value_u64, rho, recipient, sender_id, cm_ins);
    let k = view_kdf(&fvk_obj, cm);
    let mut ct = [0u8; NOTE_PLAIN_LEN_SPEND_V2];
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

    Ok((att, enc))
}
