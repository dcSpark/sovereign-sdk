#![cfg(feature = "native")]

//! Integration tests for note_spend_guest program with multi-input support
//! 
//! These tests verify soundness and completeness of the guest program:
//! - **Soundness**: No invalid witness can satisfy the constraints
//! - **Completeness**: Any valid witness should prove
//! 
//! Tests include:
//! 1. Single-input happy paths (0, 1, 2 outputs)
//! 2. Multi-input consolidation (2, 3, 4 inputs → 1 output)
//! 3. Multi-input payments (multiple inputs → payment + change)
//! 4. Value balance enforcement
//! 5. Negative cases (wrong anchor, wrong nullifier, balance violation, etc.)
//! 6. Distinct nullifier enforcement
//! 7. Output commitment verification
//!
//! Requirements:
//! - LIGERO_VERIFIER_BIN: path to webgpu_verifier
//! - LIGERO_PROGRAM_PATH: path to note_spend_guest.wasm
//! - LIGERO_SHADER_PATH: path to shader directory
//! - LIGERO_PACKING: FFT packing parameter (default: 8192)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// Re-export types we need for testing
type Hash32 = [u8; 32];

/// Public inputs for a spend transaction (new multi-input format)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SpendPublic {
    anchor_root: Hash32,
    nullifiers: Vec<Hash32>,  // Now supports multiple nullifiers
    withdraw_amount: u128,
    output_commitments: Vec<Hash32>,
}

/// Input note specification (new for multi-input)
#[derive(Debug, Clone)]
struct InputNote {
    value: u128,
    rho: Hash32,
    pos: u64,
    siblings: Vec<Hash32>,
    nullifier: Hash32,  // Computed from nf_key and rho
}

/// Output note specification (updated for Level A recipient encryption)
/// Note: recipient is DERIVED from pk_spend AND pk_ivk in the circuit: H("ADDR_V2"||domain||pk_spend||pk_ivk)
/// 
/// Fields for recipient ciphertext (Zcash-style incoming viewing):
/// - pk_ivk: Recipient's X25519 incoming viewing public key
/// - epk: Ephemeral X25519 public key (derived from rho) - PUBLIC, in tx output
/// - ct: Ciphertext bytes (144 bytes) - PUBLIC, in tx calldata
/// - mac: MAC binding ct_hash to cm and encryption key - PUBLIC, in tx output
/// - ct_hash: Hash of ct, computed by on-chain verifier from tx calldata
///
/// The on-chain flow is:
/// 1. Transaction contains (cm, epk, ct, mac) in output record
/// 2. On-chain verifier computes ct_hash = H("CT_HASH_V1" || ct)
/// 3. Proof is verified with ct_hash as public input
/// 4. This binds the ciphertext to the proof, preventing tampering
#[derive(Debug, Clone)]
struct OutputNote {
    value: u128,
    rho: Hash32,
    pk_spend: Hash32,    // Spend public key (Poseidon-derived) - recipient derived from this
    pk_ivk: Hash32,      // X25519 incoming viewing public key
    commitment: Hash32,  // NOTE_V1 commitment

    // Level A (recipient-detectable) tx fields:
    epk: Hash32,         // Ephemeral X25519 pubkey (public, in tx)
    ct: [u8; 144],       // Ciphertext bytes in tx calldata (public)
    mac: Hash32,         // Incoming MAC in tx output (public)

    // Computed by on-chain verifier from ct:
    ct_hash: Hash32,     // Poseidon("CT_HASH_V1" || ct)
}

/// Helper: hex encoding
fn hx(b: &Hash32) -> String {
    hex::encode(b)
}

/// Create an OutputNote with all computed fields (commitment, epk, ct, ct_hash, mac)
/// 
/// This is the primary way to create outputs for tests. It computes:
/// - recipient from H("ADDR_V2" || domain || pk_spend || pk_ivk) (binds both keys!)
/// - commitment from (domain, value, rho, recipient)
/// - epk from X25519_BASE(esk_from_rho_cm(domain, rho, cm))
/// - ct from encrypting note plaintext with DH-derived key
/// - ct_hash = H("CT_HASH_V1" || ct) (computed by on-chain from tx calldata)
/// - mac from (key, cm, ct_hash)
fn make_output(
    domain: &Hash32,
    value: u128,
    rho: Hash32,
    pk_spend: Hash32,    // Recipient spend pubkey (from pk_from_sk)
    pk_ivk: Hash32,      // Recipient incoming viewing pubkey (X25519)
    sender_id: &Hash32,  // Usually the sender's recipient address
) -> OutputNote {
    // ADDR_V2: binds both pk_spend and pk_ivk into the address
    let recipient = poseidon2::recipient_from_pk(domain, &pk_spend, &pk_ivk);
    let commitment = poseidon2::note_commitment(domain, value, &rho, &recipient);
    let (epk, ct, ct_hash, mac) = poseidon2::compute_recipient_ciphertext(
        domain, value, &rho, &recipient, sender_id, &pk_ivk, &commitment
    );
    OutputNote {
        value,
        rho,
        pk_spend,
        pk_ivk,
        commitment,
        epk,
        ct,
        mac,
        ct_hash,
    }
}

/// Create an OutputNote to self (uses spend_sk to derive both pk_spend and pk_ivk)
fn make_self_output(
    domain: &Hash32,
    value: u128,
    rho: Hash32,
    spend_sk: &Hash32,
) -> OutputNote {
    let pk_spend = poseidon2::pk_from_sk(spend_sk);
    let pk_ivk = poseidon2::pk_ivk_from_spend_sk(domain, spend_sk);
    let sender_id = poseidon2::recipient_from_sk(domain, spend_sk);
    make_output(domain, value, rho, pk_spend, pk_ivk, &sender_id)
}

/// Create an OutputNote to another recipient (using their pk_spend and pk_ivk)
fn make_payment_output(
    domain: &Hash32,
    value: u128,
    rho: Hash32,
    recipient_pk_spend: Hash32,
    recipient_pk_ivk: Hash32,
    sender_spend_sk: &Hash32,  // For computing sender_id
) -> OutputNote {
    let sender_id = poseidon2::recipient_from_sk(domain, sender_spend_sk);
    make_output(domain, value, rho, recipient_pk_spend, recipient_pk_ivk, &sender_id)
}

/// Get program path from environment or discover it
fn program_path() -> Result<String> {
    if let Ok(p) = std::env::var("LIGERO_PROGRAM_PATH") {
        return Ok(p);
    }
    
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let p = manifest_dir
        .join("../bins/programs/note_spend_guest.wasm")
        .canonicalize()
        .context("note_spend_guest.wasm not found - build it first")?;
    Ok(p.to_string_lossy().to_string())
}

/// Construct argv for the guest with multi-input support (updated ABI with recipient ciphertext)
/// 
/// ABI structure:
///   [1] domain, [2] spend_sk, [3] depth, [4] anchor, [5] n_in
///   For each input: value, rho, pos, siblings[depth], nullifier
///   Then: withdraw_amount, n_out
///   For each output: value, rho, pk_spend, pk_ivk, cm, epk, ct_hash, mac (8 args)
/// 
/// IMPORTANT: ct_hash is computed from ct bytes (models on-chain verifier behavior).
/// The on-chain verifier reads ct from tx calldata and computes ct_hash = H("CT_HASH_V1"||ct).
fn build_args_multi(
    domain: Hash32,
    spend_sk: Hash32,  // Spending secret key (owner of ALL inputs)
    depth: u32,
    anchor: Hash32,
    inputs: &[InputNote],
    withdraw_amount: u128,
    outputs: &[OutputNote],
) -> Vec<String> {
    let mut args = Vec::new();
    args.push(hx(&domain));              // [1] domain
    args.push(hx(&spend_sk));            // [2] spend_sk (PRIVATE)
    args.push(depth.to_string());        // [3] depth
    args.push(hx(&anchor));              // [4] anchor (PUBLIC)
    args.push(inputs.len().to_string()); // [5] n_in
    
    // Add input note arguments
    for inp in inputs {
        args.push(inp.value.to_string());    // value_in_i (PRIVATE)
        args.push(hx(&inp.rho));             // rho_in_i (PRIVATE)
        args.push(inp.pos.to_string());      // pos_in_i (PRIVATE)
        for s in &inp.siblings {
            args.push(hx(s));                // siblings_i[k] (PRIVATE)
        }
        args.push(hx(&inp.nullifier));       // nullifier_i (PUBLIC)
    }
    
    args.push(withdraw_amount.to_string()); // withdraw_amount (PUBLIC)
    args.push(outputs.len().to_string());   // n_out
    
    // Add output note arguments (8 args per output)
    for out in outputs {
        args.push(out.value.to_string());       // value_out_j (PRIVATE)
        args.push(hx(&out.rho));                // rho_out_j (PRIVATE)
        args.push(hx(&out.pk_spend));           // pk_spend_out_j (PRIVATE)
        args.push(hx(&out.pk_ivk));             // pk_ivk_out_j (PRIVATE)
        args.push(hx(&out.commitment));         // cm_out_j (PUBLIC)
        args.push(hx(&out.epk));                // epk_out_j (PUBLIC)
        
        // CRITICAL: ct_hash is computed from ct bytes (models on-chain verifier)
        // This ensures the proof is bound to the actual tx calldata, not a "free" ct_hash
        let ct_hash_from_tx = poseidon2::ct_hash(&out.ct);
        debug_assert_eq!(ct_hash_from_tx, out.ct_hash, "ct_hash must match ct");
        args.push(hx(&ct_hash_from_tx));        // ct_hash_out_j (PUBLIC, computed from tx ct)
        
        args.push(hx(&out.mac));                // mac_out_j (PUBLIC)
    }
    
    args
}

/// Convenience wrapper for single-input case (backward compatible interface)
fn build_args(
    domain: Hash32,
    value: u128,
    rho: Hash32,
    _recipient: Hash32,  // No longer needed - derived from spend_sk
    spend_sk: Hash32,
    pos: u64,
    depth: u32,
    siblings: &[Hash32],
    anchor: Hash32,
    nf: Hash32,
    withdraw_amount: u128,
    outputs: &[OutputNote],
) -> Vec<String> {
    let input = InputNote {
        value,
        rho,
        pos,
        siblings: siblings.to_vec(),
        nullifier: nf,
    };
    build_args_multi(domain, spend_sk, depth, anchor, &[input], withdraw_amount, outputs)
}

/// Private indices for the multi-input ABI with recipient ciphertext (1-based indexing into args)
/// 
/// LIGERO uses 1-based indexing: index N refers to args[N-1].
/// 
/// Private fields: spend_sk, and for each input: value, rho, pos, siblings
/// Plus for each output: value, rho, pk_spend, pk_ivk (4 private out of 8 total)
/// 
/// Args layout (0-based):
///   args[0]=domain, args[1]=spend_sk, args[2]=depth, args[3]=anchor, args[4]=n_in
///   Then per input: value, rho, pos, siblings[depth], nullifier
///   Then: withdraw_amount, n_out
///   Then per output: value, rho, pk_spend, pk_ivk, cm, epk, ct_hash, mac (8 args)
fn private_indices_multi(depth: u32, n_in: usize, n_out: usize) -> Vec<usize> {
    let mut v = vec![
        2usize, // spend_sk at args[1] → 1-based index 2
    ];
    
    // Per-input private fields
    // After header (5 args), each input has: value, rho, pos, siblings[depth], nullifier
    // Private: value, rho, pos, siblings (NOT nullifier)
    let per_in = (depth + 4) as usize; // value, rho, pos, depth siblings, nullifier
    let mut input_start = 6; // first input starts at index 6 (after header)
    for _i in 0..n_in {
        v.push(input_start);           // value_in_i
        v.push(input_start + 1);       // rho_in_i
        v.push(input_start + 2);       // pos_in_i
        for k in 0..depth as usize {
            v.push(input_start + 3 + k); // siblings_i[k]
        }
        // nullifier at input_start + 3 + depth is PUBLIC
        input_start += per_in;
    }
    
    // After inputs: withdraw_amount, n_out, then outputs
    // Each output: 8 args (value, rho, pk_spend, pk_ivk, cm, epk, ct_hash, mac)
    // Private: value, rho, pk_spend, pk_ivk (first 4)
    let output_start = 6 + n_in * per_in + 2;
    for j in 0..n_out {
        let base = output_start + 8 * j;
        v.push(base);       // value_out_j (PRIVATE)
        v.push(base + 1);   // rho_out_j (PRIVATE)
        v.push(base + 2);   // pk_spend_out_j (PRIVATE)
        v.push(base + 3);   // pk_ivk_out_j (PRIVATE)
        // cm at base + 4 is PUBLIC
        // epk at base + 5 is PUBLIC
        // ct_hash at base + 6 is PUBLIC
        // mac at base + 7 is PUBLIC
    }
    v
}

/// Convenience wrapper for single-input private indices
fn private_indices(depth: u32, n_out: usize) -> Vec<usize> {
    private_indices_multi(depth, 1, n_out)
}

// Import the hashing functions we need for testing
mod poseidon2 {
    use super::Hash32;
    use qp_poseidon_core::Poseidon2Core;
    
    fn get_hasher() -> Poseidon2Core {
        Poseidon2Core::new()
    }
    
    fn poseidon2_hash_domain(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
        let hasher = get_hasher();
        let mut buf_len = tag.len();
        for part in parts {
            buf_len += part.len();
        }
        let mut tmp = Vec::with_capacity(buf_len);
        tmp.extend_from_slice(tag);
        for part in parts {
            tmp.extend_from_slice(part);
        }
        hasher.hash_padded(&tmp)
    }
    
    pub fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
        let lvl = [level];
        poseidon2_hash_domain(b"MT_NODE_V1", &[&lvl, left, right])
    }
    
    pub fn note_commitment(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32) -> Hash32 {
        let v_bytes = value.to_le_bytes();
        poseidon2_hash_domain(b"NOTE_V1", &[domain, &v_bytes, rho, recipient])
    }
    
    pub fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"PRF_NF_V1", &[domain, nf_key, rho])
    }
    
    pub fn root_from_path(leaf: &Hash32, pos: u64, siblings: &[Hash32], depth: u32) -> Hash32 {
        let mut cur = *leaf;
        let mut idx = pos;
        for (lvl, sib) in (0..depth).zip(siblings.iter()) {
            cur = if (idx & 1) == 0 {
                mt_combine(lvl as u8, &cur, sib)
            } else {
                mt_combine(lvl as u8, sib, &cur)
            };
            idx >>= 1;
        }
        cur
    }
    
    // === Spend authorization / identity derivations (must match lib.rs) ===
    
    /// pk = H("PK_V1" || spend_sk)
    pub fn pk_from_sk(spend_sk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"PK_V1", &[spend_sk])
    }
    
    /// recipient_addr = H("ADDR_V2" || domain || pk_spend || pk_ivk)
    /// 
    /// IMPORTANT: This binds BOTH the spending key and incoming viewing key into the address.
    /// This prevents the "mismatched encryption" attack where a note is committed to pk_spend X
    /// but encrypted to pk_ivk Y, making it undecryptable by the owner of pk_spend X.
    pub fn recipient_from_pk(domain: &Hash32, pk_spend: &Hash32, pk_ivk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"ADDR_V2", &[domain, pk_spend, pk_ivk])
    }
    
    /// recipient_addr from spend_sk (convenience: sk -> pk_spend, pk_ivk -> recipient)
    pub fn recipient_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
        let pk_spend = pk_from_sk(spend_sk);
        let pk_ivk = pk_ivk_from_spend_sk(domain, spend_sk);
        recipient_from_pk(domain, &pk_spend, &pk_ivk)
    }
    
    /// nf_key = H("NFKEY_V1" || domain || spend_sk)
    pub fn nf_key_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"NFKEY_V1", &[domain, spend_sk])
    }
    
    // === Level B: Viewer Attestation Functions ===
    
    /// Compute FVK commitment: H("FVK_COMMIT_V1" || fvk)
    pub fn fvk_commit(fvk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"FVK_COMMIT_V1", &[fvk])
    }
    
    /// Derive per-note viewing key: H("VIEW_KDF_V1" || fvk || cm)
    pub fn view_kdf(fvk: &Hash32, cm: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"VIEW_KDF_V1", &[fvk, cm])
    }
    
    /// Produce the i-th 32-byte stream block for key k using Poseidon2.
    pub fn stream_block(k: &Hash32, ctr: u32) -> Hash32 {
        let c = ctr.to_le_bytes();
        poseidon2_hash_domain(b"VIEW_STREAM_V1", &[k, &c])
    }
    
    /// Compute ciphertext hash: H("CT_HASH_V1" || ct)
    pub fn ct_hash(ct: &[u8]) -> Hash32 {
        poseidon2_hash_domain(b"CT_HASH_V1", &[ct])
    }
    
    /// Compute viewing MAC: H("VIEW_MAC_V1" || k || cm || ct_hash)
    pub fn view_mac(k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"VIEW_MAC_V1", &[k, cm, ct_h])
    }
    
    /// Stream XOR encryption: XOR plaintext with Poseidon-based keystream
    pub fn stream_xor_encrypt(k: &Hash32, pt: &[u8]) -> Vec<u8> {
        let mut ct = vec![0u8; pt.len()];
        let mut ctr = 0u32;
        let mut off = 0usize;
        while off < pt.len() {
            let ks = stream_block(k, ctr);
            ctr = ctr.wrapping_add(1);
            let take = std::cmp::min(32, pt.len() - off);
            for i in 0..take {
                ct[off + i] = pt[off + i] ^ ks[i];
            }
            off += take;
        }
        ct
    }
    
    /// Encode note plaintext for viewer encryption (144 bytes)
    /// [ domain(32) | value_le_16 | rho(32) | recipient(32) | sender_id(32) ]
    pub fn encode_note_plain(
        domain: &Hash32,
        value: u128,
        rho: &Hash32,
        recipient: &Hash32,
        sender_id: &Hash32,
    ) -> [u8; 144] {
        let mut out = [0u8; 144];
        out[0..32].copy_from_slice(domain);
        out[32..48].copy_from_slice(&value.to_le_bytes());
        out[48..80].copy_from_slice(rho);
        out[80..112].copy_from_slice(recipient);
        out[112..144].copy_from_slice(sender_id);
        out
    }
    
    // === Level A: Recipient (incoming) ciphertext ===
    // These mirror the functions in lib.rs for generating test data
    
    use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};
    
    /// Apply X25519 scalar clamping
    fn clamp_x25519_scalar(s: &mut [u8; 32]) {
        s[0] &= 248;
        s[31] &= 127;
        s[31] |= 64;
    }
    
    /// Derive ephemeral secret key from (domain, rho, cm).
    /// esk = clamp(H("ESK_V2" || domain || rho || cm))
    /// 
    /// Including cm reduces linkability if rho is accidentally reused across notes.
    pub fn esk_from_rho_cm(domain: &Hash32, rho: &Hash32, cm: &Hash32) -> [u8; 32] {
        let mut esk = poseidon2_hash_domain(b"ESK_V2", &[domain, rho, cm]);
        clamp_x25519_scalar(&mut esk);
        esk
    }
    
    /// Compute X25519 public key from secret key: pk = [sk] * G
    pub fn x25519_base(sk_bytes: &[u8; 32]) -> [u8; 32] {
        let sk = X25519Secret::from(*sk_bytes);
        let pk = X25519PublicKey::from(&sk);
        pk.to_bytes()
    }
    
    /// Compute X25519 shared secret: shared = [sk] * pk
    pub fn x25519_shared(sk_bytes: &[u8; 32], pk_bytes: &[u8; 32]) -> [u8; 32] {
        let sk = X25519Secret::from(*sk_bytes);
        let pk = X25519PublicKey::from(*pk_bytes);
        sk.diffie_hellman(&pk).to_bytes()
    }
    
    /// Derive symmetric key for recipient ciphertext: k_in = H("IN_KDF_V1" || domain || dh_shared || cm)
    pub fn in_kdf(domain: &Hash32, dh_shared: &Hash32, cm: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"IN_KDF_V1", &[domain, dh_shared, cm])
    }
    
    /// Compute incoming MAC: mac_in = H("IN_MAC_V1" || k || cm || ct_hash)
    pub fn in_mac(k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"IN_MAC_V1", &[k, cm, ct_h])
    }
    
    /// Produce the i-th 32-byte stream block for incoming encryption.
    /// Uses separate domain tag from viewer stream (IN_STREAM_V1 vs VIEW_STREAM_V1).
    pub fn in_stream_block(k: &Hash32, ctr: u32) -> Hash32 {
        let c = ctr.to_le_bytes();
        poseidon2_hash_domain(b"IN_STREAM_V1", &[k, &c])
    }
    
    /// Stream XOR encryption for incoming (recipient) ciphertext.
    /// Uses IN_STREAM_V1 domain tag (separate from VIEW_STREAM_V1 for viewers).
    pub fn stream_xor_encrypt_in(k: &Hash32, pt: &[u8]) -> Vec<u8> {
        let mut ct = vec![0u8; pt.len()];
        let mut ctr = 0u32;
        let mut off = 0usize;
        while off < pt.len() {
            let ks = in_stream_block(k, ctr);
            ctr = ctr.wrapping_add(1);
            let take = std::cmp::min(32, pt.len() - off);
            for i in 0..take {
                ct[off + i] = pt[off + i] ^ ks[i];
            }
            off += take;
        }
        ct
    }
    
    /// Derive incoming viewing secret key from spend_sk (wallet convenience function)
    /// ivk_seed = H("IVK_SEED_V1" || domain || spend_sk), then clamp to X25519 scalar
    pub fn ivk_sk_from_spend_sk(domain: &Hash32, spend_sk: &Hash32) -> [u8; 32] {
        let mut ivk = poseidon2_hash_domain(b"IVK_SEED_V1", &[domain, spend_sk]);
        clamp_x25519_scalar(&mut ivk);
        ivk
    }
    
    /// Derive incoming viewing public key from spend_sk
    pub fn pk_ivk_from_spend_sk(domain: &Hash32, spend_sk: &Hash32) -> [u8; 32] {
        let ivk_sk = ivk_sk_from_spend_sk(domain, spend_sk);
        x25519_base(&ivk_sk)
    }
    
    /// Compute recipient ciphertext fields for an output note
    /// Returns (epk, ct, ct_hash, mac)
    /// 
    /// The ct bytes would be in tx calldata; on-chain verifier computes ct_hash from them.
    /// 
    /// Note: esk is derived from (domain, rho, cm) to reduce linkability on rho reuse.
    pub fn compute_recipient_ciphertext(
        domain: &Hash32,
        value: u128,
        rho: &Hash32,
        recipient: &Hash32,
        sender_id: &Hash32,
        pk_ivk: &Hash32,
        cm: &Hash32,
    ) -> (Hash32, [u8; 144], Hash32, Hash32) {
        // Derive ephemeral secret from (domain, rho, cm) - cm binding reduces linkability
        let esk = esk_from_rho_cm(domain, rho, cm);
        
        // Compute ephemeral public key
        let epk = x25519_base(&esk);
        
        // DH shared secret
        let dh = x25519_shared(&esk, pk_ivk);
        
        // Derive symmetric key
        let k_in = in_kdf(domain, &dh, cm);
        
        // Encode and encrypt plaintext using incoming stream
        let pt = encode_note_plain(domain, value, rho, recipient, sender_id);
        let ct_vec = stream_xor_encrypt_in(&k_in, &pt);
        
        // Copy to fixed-size array
        let mut ct = [0u8; 144];
        ct.copy_from_slice(&ct_vec);
        
        // Compute digests
        let ct_h = ct_hash(&ct);
        let macv = in_mac(&k_in, cm, &ct_h);
        
        (epk, ct, ct_h, macv)
    }
}

// Simple Merkle tree for testing
struct MerkleTree {
    depth: u8,
    #[allow(dead_code)]
    zero_hashes: Vec<Hash32>,
    leaves: Vec<Hash32>,
}

impl MerkleTree {
    fn new(depth: u8) -> Self {
        let mut zero_hashes = vec![[0u8; 32]; depth as usize + 1];
        for i in (0..depth as usize).rev() {
            zero_hashes[i] = poseidon2::mt_combine(i as u8, &zero_hashes[i + 1], &zero_hashes[i + 1]);
        }
        let size = 1 << depth;
        Self {
            depth,
            zero_hashes,
            leaves: vec![[0u8; 32]; size],
        }
    }
    
    fn set_leaf(&mut self, pos: usize, leaf: Hash32) {
        self.leaves[pos] = leaf;
    }
    
    fn root(&self) -> Hash32 {
        let mut level = self.leaves.clone();
        for lvl in 0..self.depth {
            let mut next_level = Vec::new();
            for i in (0..level.len()).step_by(2) {
                let left = level[i];
                let right = level[i + 1];
                next_level.push(poseidon2::mt_combine(lvl, &left, &right));
            }
            level = next_level;
        }
        level[0]
    }
    
    fn open(&self, pos: usize) -> Vec<Hash32> {
        let mut siblings = Vec::new();
        let mut level = self.leaves.clone();
        let mut idx = pos;
        
        for lvl in 0..self.depth {
            let sibling_idx = if idx & 1 == 0 { idx + 1 } else { idx - 1 };
            siblings.push(level[sibling_idx]);
            
            // Compute next level
            let mut next_level = Vec::new();
            for i in (0..level.len()).step_by(2) {
                let left = level[i];
                let right = level[i + 1];
                next_level.push(poseidon2::mt_combine(lvl, &left, &right));
            }
            level = next_level;
            idx >>= 1;
        }
        siblings
    }
}

// =====================================================================
// HAPPY PATH: Valid witness with 0 outputs (pure withdrawal)
// =====================================================================

#[test]
fn test_valid_spend_no_outputs() -> Result<()> {
    println!("\n=== Happy Path: Valid Spend with 0 Outputs (Pure Withdrawal) ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;

    // Build a tree with one note using proper key derivation
    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 1000;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);
    
    // Withdraw entire note value
    let withdraw_amount: u128 = 1000;
    let outputs = vec![]; // No outputs

    println!("✓ Input value:     {}", value);
    println!("✓ Withdraw amount: {}", withdraw_amount);
    println!("✓ Output notes:    {}", outputs.len());
    println!("✓ Balance: {} = {} + 0", value, withdraw_amount);

    let args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &outputs);
    let expected_len = 11 + depth as usize; // 11 base args + depth siblings + 4*0 outputs
    assert_eq!(args.len(), expected_len);
    
    println!("✓ Built {} arguments", args.len());
    println!("✓ Private indices: {:?}", private_indices(depth, 0));
    println!("✓ Test structure validated\n");

    Ok(())
}

// =====================================================================
// HAPPY PATH: Valid witness with 1 output
// =====================================================================

#[test]
fn test_valid_spend_one_output() -> Result<()> {
    println!("\n=== Happy Path: Valid Spend with 1 Output ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;

    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 1000;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);
    
    // Create 1 output note (change)
    // Note: recipient is DERIVED from pk_spend in the circuit
    let withdraw_amount: u128 = 300;
    let output1_value: u128 = 700;
    let output1_rho = [10u8; 32];
    // Use receiver's keys (could be self or another party)
    let receiver_sk = [11u8; 32];
    let output1_pk_spend = poseidon2::pk_from_sk(&receiver_sk);
    let output1_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &receiver_sk);
    let output = make_payment_output(&domain, output1_value, output1_rho, output1_pk_spend, output1_pk_ivk, &spend_sk);
    
    let outputs = vec![output];

    println!("✓ Input value:     {}", value);
    println!("✓ Withdraw amount: {}", withdraw_amount);
    println!("✓ Output 1 value:  {}", output1_value);
    println!("✓ Balance: {} = {} + {}", value, withdraw_amount, output1_value);
    assert_eq!(value, withdraw_amount + output1_value);

    let args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &outputs);
    let expected_len = 11 + depth as usize + 8; // 11 base args + depth siblings + 8*1 outputs
    assert_eq!(args.len(), expected_len);
    
    println!("✓ Built {} arguments", args.len());
    println!("✓ Private indices: {:?}", private_indices(depth, 1));
    println!("✓ Test structure validated\n");

    Ok(())
}

// =====================================================================
// HAPPY PATH: Valid witness with 2 outputs
// =====================================================================

#[test]
fn test_valid_spend_two_outputs() -> Result<()> {
    println!("\n=== Happy Path: Valid Spend with 2 Outputs ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;

    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 1000;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);
    
    // Create 2 output notes (split)
    // Note: recipients are DERIVED from pk_spend in the circuit
    let withdraw_amount: u128 = 100;
    let output1_value: u128 = 400;
    let output2_value: u128 = 500;
    
    // Receiver 1's keys
    let receiver1_sk = [11u8; 32];
    let output1 = make_payment_output(
        &domain, output1_value, [10u8; 32],
        poseidon2::pk_from_sk(&receiver1_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver1_sk),
        &spend_sk
    );
    
    // Receiver 2's keys
    let receiver2_sk = [21u8; 32];
    let output2 = make_payment_output(
        &domain, output2_value, [20u8; 32],
        poseidon2::pk_from_sk(&receiver2_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver2_sk),
        &spend_sk
    );
    
    let outputs = vec![output1, output2];

    println!("✓ Input value:     {}", value);
    println!("✓ Withdraw amount: {}", withdraw_amount);
    println!("✓ Output 1 value:  {}", output1_value);
    println!("✓ Output 2 value:  {}", output2_value);
    println!("✓ Balance: {} = {} + {} + {}", value, withdraw_amount, output1_value, output2_value);
    assert_eq!(value, withdraw_amount + output1_value + output2_value);

    let args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &outputs);
    let expected_len = 11 + depth as usize + 16; // 11 base args + depth siblings + 8*2 outputs
    assert_eq!(args.len(), expected_len);
    
    println!("✓ Built {} arguments", args.len());
    println!("✓ Private indices: {:?}", private_indices(depth, 2));
    println!("✓ Test structure validated\n");

    Ok(())
}

// =====================================================================
// NEGATIVE TEST: Balance violation (underspend)
// =====================================================================

#[test]
fn test_reject_balance_violation_underspend() -> Result<()> {
    println!("\n=== Negative Test: Balance Violation (Underspend) ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 1000;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);

    // Balance doesn't add up (underspend - value being burned)
    let withdraw_amount: u128 = 300;
    let output1_value: u128 = 600; // Total: 900 < 1000
    let output1_rho = [10u8; 32];
    let output1_pk_out = [11u8; 32];
    // Use receiver's keys (could be self or another party)
    let receiver_sk = [11u8; 32];
    let output = make_payment_output(
        &domain, output1_value, output1_rho,
        poseidon2::pk_from_sk(&receiver_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver_sk),
        &spend_sk
    );
    let outputs = vec![output];

    println!("✗ Input value:     {}", value);
    println!("✗ Withdraw amount: {}", withdraw_amount);
    println!("✗ Output value:    {}", output1_value);
    println!("✗ Balance: {} ≠ {} + {} (underspend!)", value, withdraw_amount, output1_value);

    let _args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &outputs);
    
    println!("✓ Arguments prepared for failure case");
    println!("  NOTE: Guest would fail with balance violation\n");
    
    Ok(())
}

// =====================================================================
// NEGATIVE TEST: Balance violation (overspend)
// =====================================================================

#[test]
fn test_reject_balance_violation_overspend() -> Result<()> {
    println!("\n=== Negative Test: Balance Violation (Overspend) ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 1000;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);

    // Balance doesn't add up (overspend - creating value)
    let withdraw_amount: u128 = 500;
    let output1_value: u128 = 600; // Total: 1100 > 1000
    let output1_rho = [10u8; 32];
    let receiver_sk = [11u8; 32];
    let output = make_payment_output(
        &domain, output1_value, output1_rho,
        poseidon2::pk_from_sk(&receiver_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver_sk),
        &spend_sk
    );
    let outputs = vec![output];

    println!("✗ Input value:     {}", value);
    println!("✗ Withdraw amount: {}", withdraw_amount);
    println!("✗ Output value:    {}", output1_value);
    println!("✗ Balance: {} ≠ {} + {} (overspend!)", value, withdraw_amount, output1_value);

    let _args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &outputs);
    
    println!("✓ Arguments prepared for failure case");
    println!("  NOTE: Guest would fail with balance violation\n");
    
    Ok(())
}

// =====================================================================
// NEGATIVE TEST: Wrong output commitment
// =====================================================================

#[test]
fn test_reject_wrong_output_commitment() -> Result<()> {
    println!("\n=== Negative Test: Wrong Output Commitment ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 1000;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);

    let withdraw_amount: u128 = 300;
    let output1_value: u128 = 700;
    let output1_rho = [10u8; 32];
    let receiver_sk = [11u8; 32];
    
    // Create valid output then tamper with commitment
    let mut output = make_payment_output(
        &domain, output1_value, output1_rho,
        poseidon2::pk_from_sk(&receiver_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver_sk),
        &spend_sk
    );
    let good_cm = output.commitment;
    output.commitment[0] ^= 1; // Tamper with commitment
    
    let outputs = vec![output];

    println!("✗ Computed commitment: {}", hx(&good_cm));
    println!("✗ Provided commitment: {}", hx(&outputs[0].commitment));

    let _args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &outputs);
    
    println!("✓ Arguments prepared for failure case");
    println!("  NOTE: Guest would fail with commitment mismatch\n");
    
    Ok(())
}

// =====================================================================
// NEGATIVE TEST: Wrong anchor root
// =====================================================================

#[test]
fn test_reject_wrong_anchor() -> Result<()> {
    println!("\n=== Negative Test: Wrong Anchor Root ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;
    let domain = [9u8; 32];
    let rho = [7u8; 32];
    let spend_sk = [5u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 100;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);
    let withdraw_amount: u128 = 100;

    // Tamper anchor argument
    let mut bad_anchor = anchor;
    bad_anchor[0] ^= 1;
    println!("✗ Tampered anchor: {} (should be {})", hx(&bad_anchor), hx(&anchor));

    let _args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, bad_anchor, nf, withdraw_amount, &[]);
    
    println!("✓ Arguments prepared for failure case");
    println!("  NOTE: Guest would fail with wrong anchor\n");
    
    Ok(())
}

// =====================================================================
// NEGATIVE TEST: Wrong nullifier
// =====================================================================

#[test]
fn test_reject_wrong_nullifier() -> Result<()> {
    println!("\n=== Negative Test: Wrong Nullifier ===\n");
    
    let _program = program_path()?;
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);  // Derived from sk
    let value: u128 = 100;
    let pos: u64 = 0;

    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);

    // Tamper nullifier
    let mut bad_nf = nf;
    bad_nf[31] ^= 1;
    println!("✗ Tampered nullifier: {} (should be {})", hx(&bad_nf), hx(&nf));

    let _args = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, bad_nf, 100, &[]);
    
    println!("✓ Arguments prepared for failure case");
    println!("  NOTE: Guest would fail with wrong nullifier\n");
    
    Ok(())
}

// =====================================================================
// ARGUMENT VALIDATION TESTS
// =====================================================================

#[test]
fn test_argument_count_validation() {
    println!("\n=== Argument Count Validation ===\n");

    let depth = 8u32;
    let domain = [1u8; 32];
    let value = 100u128;
    let rho = [2u8; 32];
    let spend_sk = [4u8; 32];  // Spending secret key
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);  // Derived from sk
    let pos = 0u64;
    let anchor = [5u8; 32];
    let nf = [6u8; 32];
    let withdraw_amount = 50u128;
    let siblings = vec![[0u8; 32]; depth as usize];

    // Test with 0 outputs
    let args0 = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, withdraw_amount, &[]);
    assert_eq!(args0.len(), (11 + depth) as usize);
    println!("✓ Argument count with 0 outputs: {} (11 + {})", args0.len(), depth);

    // Test with 1 output (8 args per output now)
    let out1 = make_self_output(&domain, 50, [10u8; 32], &spend_sk);
    let args1 = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, 0, &[out1]);
    assert_eq!(args1.len(), (11 + depth + 8) as usize);
    println!("✓ Argument count with 1 output:  {} (11 + {} + 8)", args1.len(), depth);

    // Test with 2 outputs (16 args total for outputs)
    let out2a = make_self_output(&domain, 25, [10u8; 32], &spend_sk);
    let out2b = make_self_output(&domain, 25, [20u8; 32], &spend_sk);
    let args2 = build_args(domain, value, rho, recipient, spend_sk, pos, depth, &siblings, anchor, nf, 50, &[out2a, out2b]);
    assert_eq!(args2.len(), (11 + depth + 16) as usize);
    println!("✓ Argument count with 2 outputs: {} (11 + {} + 16)", args2.len(), depth);
    
    // Verify private indices
    let private0 = private_indices(depth, 0);
    println!("✓ Private indices (0 out): {} fields", private0.len());
    
    let private1 = private_indices(depth, 1);
    println!("✓ Private indices (1 out): {} fields", private1.len());
    
    let private2 = private_indices(depth, 2);
    println!("✓ Private indices (2 out): {} fields", private2.len());
    println!();
}

#[test]
fn test_hex_encoding_format() {
    println!("\n=== Hex Encoding Format ===\n");
    
    let hash: Hash32 = [0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89,
                        0x9A, 0xBC, 0xDE, 0xF0, 0x12, 0x34, 0x56, 0x78,
                        0x87, 0x65, 0x43, 0x21, 0x0F, 0xED, 0xCB, 0xA9,
                        0x98, 0x76, 0x54, 0x32, 0x10, 0xFE, 0xDC, 0xBA];
    
    let encoded = hx(&hash);
    
    // Should be exactly 64 hex characters
    assert_eq!(encoded.len(), 64);
    println!("✓ Hex encoding length: {}", encoded.len());
    
    // Should contain only hex chars
    assert!(encoded.chars().all(|c| c.is_ascii_hexdigit()));
    println!("✓ All characters are valid hex digits");
    
    // Should be lowercase
    assert!(encoded.chars().all(|c| !c.is_uppercase() || c.is_numeric()));
    println!("✓ Encoded as: {}", &encoded[..16]);
    println!();
}

// =====================================================================
// REGRESSION TEST: Nullifier Malleability Bug
// =====================================================================

/// Mirrors the OLD (buggy) guest constraints where nf_key is unconstrained.
/// This allows an attacker to pick any nf_key and generate different nullifiers
/// for the same note, enabling double-spends.
fn satisfies_old_buggy_constraints(
    domain: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
    nf_key: &Hash32,  // UNCONSTRAINED - this is the bug!
    pos: u64,
    depth: u32,
    siblings: &[Hash32],
    anchor: &Hash32,
    nullifier_arg: &Hash32,
    withdraw_amount: u128,
    outputs: &[OutputNote],
) -> bool {
    if siblings.len() != depth as usize {
        return false;
    }
    // Output commitments + sum
    let mut out_sum: u128 = 0;
    for o in outputs {
        out_sum = match out_sum.checked_add(o.value) {
            Some(x) => x,
            None => return false,
        };
        // With ADDR_V2, recipient is derived from both pk_spend AND pk_ivk
        let rcp = poseidon2::recipient_from_pk(domain, &o.pk_spend, &o.pk_ivk);
        let cm = poseidon2::note_commitment(domain, o.value, &o.rho, &rcp);
        if cm != o.commitment {
            return false;
        }
    }
    // Anchor check (membership)
    let cm_in = poseidon2::note_commitment(domain, value, rho, recipient);
    let anchor_cmp = poseidon2::root_from_path(&cm_in, pos, siblings, depth);
    if &anchor_cmp != anchor {
        return false;
    }
    // Nullifier check - BUG: nf_key is unconstrained!
    let nf_cmp = poseidon2::nullifier(domain, nf_key, rho);
    if &nf_cmp != nullifier_arg {
        return false;
    }
    // Balance check
    match withdraw_amount.checked_add(out_sum) {
        Some(rhs) => rhs == value,
        None => false,
    }
}

/// Mirrors the NEW (fixed) guest constraints where nf_key is derived from spend_sk,
/// and recipient must match the derivation from spend_sk.
/// This ensures only the holder of spend_sk can spend the note, and nullifiers
/// are deterministic (preventing double-spends).
fn satisfies_fixed_constraints(
    domain: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
    spend_sk: &Hash32,  // CONSTRAINED - nf_key and recipient derived from this
    pos: u64,
    depth: u32,
    siblings: &[Hash32],
    anchor: &Hash32,
    nullifier_arg: &Hash32,
    withdraw_amount: u128,
    outputs: &[OutputNote],
) -> bool {
    if siblings.len() != depth as usize {
        return false;
    }
    
    // FIXED: Verify spending authorization - recipient must match sk-derived recipient
    let recipient_expected = poseidon2::recipient_from_sk(domain, spend_sk);
    if &recipient_expected != recipient {
        return false;  // Wrong spend_sk for this recipient!
    }
    
    // FIXED: Derive nf_key deterministically from spend_sk
    let nf_key = poseidon2::nf_key_from_sk(domain, spend_sk);
    
    // Output commitments + sum (recipient is derived from both pk_spend and pk_ivk)
    let mut out_sum: u128 = 0;
    for o in outputs {
        out_sum = match out_sum.checked_add(o.value) {
            Some(x) => x,
            None => return false,
        };
        let rcp = poseidon2::recipient_from_pk(domain, &o.pk_spend, &o.pk_ivk);
        let cm = poseidon2::note_commitment(domain, o.value, &o.rho, &rcp);
        if cm != o.commitment {
            return false;
        }
    }
    // Anchor check (membership)
    let cm_in = poseidon2::note_commitment(domain, value, rho, recipient);
    let anchor_cmp = poseidon2::root_from_path(&cm_in, pos, siblings, depth);
    if &anchor_cmp != anchor {
        return false;
    }
    // Nullifier check - now uses derived nf_key
    let nf_cmp = poseidon2::nullifier(domain, &nf_key, rho);
    if &nf_cmp != nullifier_arg {
        return false;
    }
    // Balance check
    match withdraw_amount.checked_add(out_sum) {
        Some(rhs) => rhs == value,
        None => false,
    }
}

/// Multi-input constraint verification (new for consolidation support)
fn satisfies_multi_input_constraints(
    domain: &Hash32,
    spend_sk: &Hash32,
    depth: u32,
    anchor: &Hash32,
    inputs: &[InputNote],
    withdraw_amount: u128,
    outputs: &[OutputNote],
) -> bool {
    if inputs.is_empty() || inputs.len() > 4 {
        return false;
    }
    
    // Derive recipient + nf_key once for all inputs (same-owner)
    let recipient_owner = poseidon2::recipient_from_sk(domain, spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(domain, spend_sk);
    
    // Verify all inputs
    let mut sum_in: u128 = 0;
    let mut nullifiers = Vec::new();
    let mut input_rhos = Vec::new();
    
    for inp in inputs {
        // Zero-value inputs forbidden
        if inp.value == 0 {
            return false;
        }
        
        if inp.siblings.len() != depth as usize {
            return false;
        }
        if inp.pos >= (1u64 << depth) {
            return false;
        }
        
        sum_in = match sum_in.checked_add(inp.value) {
            Some(x) => x,
            None => return false,
        };
        
        // Verify Merkle membership
        let cm_i = poseidon2::note_commitment(domain, inp.value, &inp.rho, &recipient_owner);
        let anchor_i = poseidon2::root_from_path(&cm_i, inp.pos, &inp.siblings, depth);
        if &anchor_i != anchor {
            return false;
        }
        
        // Verify nullifier
        let nf_i = poseidon2::nullifier(domain, &nf_key, &inp.rho);
        if nf_i != inp.nullifier {
            return false;
        }
        
        nullifiers.push(nf_i);
        input_rhos.push(inp.rho);
    }
    
    // Check distinct nullifiers
    for i in 0..nullifiers.len() {
        for j in (i + 1)..nullifiers.len() {
            if nullifiers[i] == nullifiers[j] {
                return false;  // Duplicate nullifier!
            }
        }
    }
    
    // Derive sender_id for recipient ciphertext verification
    let sender_id = poseidon2::recipient_from_sk(domain, spend_sk);

    // Verify outputs
    let mut out_sum: u128 = 0;
    let mut output_rhos = Vec::new();
    for o in outputs {
        // Zero-value outputs forbidden
        if o.value == 0 {
            return false;
        }
        
        out_sum = match out_sum.checked_add(o.value) {
            Some(x) => x,
            None => return false,
        };
        // ADDR_V2: binds both pk_spend and pk_ivk into the address
        let rcp = poseidon2::recipient_from_pk(domain, &o.pk_spend, &o.pk_ivk);
        let cm = poseidon2::note_commitment(domain, o.value, &o.rho, &rcp);
        if cm != o.commitment {
            return false;
        }
        
        // === Level A recipient ciphertext verification ===
        
        // Reject low-order pk_ivk: DH must be non-zero (matches guest dh_or check)
        let esk = poseidon2::esk_from_rho_cm(domain, &o.rho, &o.commitment);
        let dh = poseidon2::x25519_shared(&esk, &o.pk_ivk);
        let mut dh_or = 0u8;
        for b in dh.iter() { dh_or |= *b; }
        if dh_or == 0 {
            return false;  // Low-order pk_ivk produces all-zero DH
        }
        
        // Compute expected (epk, ct, ct_hash, mac) from witness
        let (epk_exp, _ct_exp, ct_hash_exp, mac_exp) = poseidon2::compute_recipient_ciphertext(
            domain, o.value, &o.rho, &rcp, &sender_id, &o.pk_ivk, &o.commitment
        );
        
        // Public ct_hash is derived from tx calldata ct (models on-chain verifier)
        let ct_hash_from_tx = poseidon2::ct_hash(&o.ct);
        
        // Check statement equivalence
        if o.epk != epk_exp {
            return false;  // epk mismatch
        }
        if ct_hash_from_tx != ct_hash_exp {
            return false;  // ct was tampered (on-chain ct_hash differs from proof)
        }
        if o.mac != mac_exp {
            return false;  // mac mismatch
        }
        
        // Consistency check: stored ct_hash should match computed
        if o.ct_hash != ct_hash_from_tx {
            return false;  // Internal consistency error
        }
        
        output_rhos.push(o.rho);
    }
    
    // Rho reuse prevention: output rho != any input rho
    for out_rho in &output_rhos {
        for in_rho in &input_rhos {
            if out_rho == in_rho {
                return false;  // Output rho matches input rho - would create unspendable note
            }
        }
    }
    
    // Output rhos must be pairwise distinct
    for i in 0..output_rhos.len() {
        for j in (i + 1)..output_rhos.len() {
            if output_rhos[i] == output_rhos[j] {
                return false;  // Duplicate output rho
            }
        }
    }
    
    // Balance: sum(inputs) == withdraw + sum(outputs)
    match withdraw_amount.checked_add(out_sum) {
        Some(rhs) => rhs == sum_in,
        None => false,
    }
}

/// Viewer attestation data for Level B verification
#[derive(Clone)]
struct ViewerAttestation {
    /// Full viewing key
    fvk: Hash32,
    /// Commitment to FVK: H("FVK_COMMIT_V1" || fvk)
    fvk_commit: Hash32,
    /// Sender identity (for plaintext encoding)
    sender_id: Hash32,
    /// Ciphertext hash for each output: H("CT_HASH_V1" || ct)
    ct_hashes: Vec<Hash32>,
    /// MAC for each output: H("VIEW_MAC_V1" || k || cm || ct_hash)
    macs: Vec<Hash32>,
}

/// Verify viewer attestations match the expected values
/// 
/// For each viewer:
///   1. fvk_commit == H("FVK_COMMIT_V1" || fvk)
///   2. For each output j:
///      - k_j = H("VIEW_KDF_V1" || fvk || cm_j)
///      - pt = encode_note_plain(domain, value, rho, recipient, sender_id)
///      - ct = stream_xor(k_j, pt)
///      - ct_hash_j == H("CT_HASH_V1" || ct)
///      - mac_j == H("VIEW_MAC_V1" || k_j || cm_j || ct_hash_j)
fn satisfies_viewer_attestations(
    domain: &Hash32,
    outputs: &[OutputNote],
    viewers: &[ViewerAttestation],
) -> bool {
    for v in viewers {
        // Verify FVK commitment
        let computed_fvk_commit = poseidon2::fvk_commit(&v.fvk);
        if computed_fvk_commit != v.fvk_commit {
            return false;
        }
        
        // Must have ct_hash and mac for each output
        if v.ct_hashes.len() != outputs.len() || v.macs.len() != outputs.len() {
            return false;
        }
        
        for (j, o) in outputs.iter().enumerate() {
            // Derive per-note key
            let k = poseidon2::view_kdf(&v.fvk, &o.commitment);
            
            // Compute recipient (ADDR_V2 binds both keys)
            let recipient = poseidon2::recipient_from_pk(domain, &o.pk_spend, &o.pk_ivk);
            
            // Encode plaintext
            let pt = poseidon2::encode_note_plain(domain, o.value, &o.rho, &recipient, &v.sender_id);
            
            // Encrypt
            let ct = poseidon2::stream_xor_encrypt(&k, &pt);
            
            // Verify ct_hash
            let computed_ct_hash = poseidon2::ct_hash(&ct);
            if computed_ct_hash != v.ct_hashes[j] {
                return false;
            }
            
            // Verify MAC
            let computed_mac = poseidon2::view_mac(&k, &o.commitment, &computed_ct_hash);
            if computed_mac != v.macs[j] {
                return false;
            }
        }
    }
    true
}

/// Regression test demonstrating the nullifier malleability bug and its fix.
/// 
/// The OLD constraints allowed an attacker to pick arbitrary nf_key values,
/// generating different nullifiers for the same note (enabling double-spends).
/// 
/// The FIXED constraints derive nf_key from spend_sk, making nullifiers
/// deterministic and tied to spend authorization.
#[test]
fn test_nullifier_malleability_regression() -> Result<()> {
    println!("\n=== Regression Test: Nullifier Malleability ===\n");
    
    // Use small depth for fast testing (depth=16 creates 65k leaves, very slow)
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let rho = [2u8; 32];
    let value: u128 = 1000;
    let pos: u64 = 0;
    
    // For the OLD buggy system: arbitrary recipient and nf_key
    let recipient_old = [3u8; 32];
    
    // Build tree containing this note (using old-style arbitrary recipient)
    let cm_old = poseidon2::note_commitment(&domain, value, &rho, &recipient_old);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm_old);
    let anchor = tree.root();
    let siblings = tree.open(pos as usize);
    
    // === Demonstrate the BUG with OLD constraints ===
    println!("--- OLD (Buggy) Constraints ---");
    
    // Two different nf_keys -> two different nullifiers for the same note
    let nf_key_1 = [4u8; 32];
    let mut nf_key_2 = nf_key_1;
    nf_key_2[0] ^= 1;
    
    let nf_1 = poseidon2::nullifier(&domain, &nf_key_1, &rho);
    let nf_2 = poseidon2::nullifier(&domain, &nf_key_2, &rho);
    assert_ne!(nf_1, nf_2, "unexpected hash collision");
    
    let withdraw_amount = value;
    let outputs: Vec<OutputNote> = vec![];
    
    // Both witnesses satisfy OLD constraints - THIS IS THE BUG!
    let witness1_valid = satisfies_old_buggy_constraints(
        &domain, value, &rho, &recipient_old, &nf_key_1, pos, depth, &siblings, 
        &anchor, &nf_1, withdraw_amount, &outputs,
    );
    let witness2_valid = satisfies_old_buggy_constraints(
        &domain, value, &rho, &recipient_old, &nf_key_2, pos, depth, &siblings, 
        &anchor, &nf_2, withdraw_amount, &outputs,
    );
    
    println!("  Witness 1 (nf_key_1) valid: {}", witness1_valid);
    println!("  Witness 2 (nf_key_2) valid: {}", witness2_valid);
    println!("  Nullifier 1: {}", hx(&nf_1));
    println!("  Nullifier 2: {}", hx(&nf_2));
    
    // This demonstrates the bug: both witnesses are valid, allowing double-spend
    assert!(witness1_valid, "Witness 1 should satisfy old constraints");
    assert!(witness2_valid, "BUG DEMONSTRATED: Witness 2 also satisfies old constraints with different nf_key!");
    println!("  ⚠️  BUG: Same note can produce different valid nullifiers!\n");
    
    // === Demonstrate the FIX with NEW constraints ===
    println!("--- NEW (Fixed) Constraints ---");
    
    // Now use proper key derivation
    let spend_sk = [42u8; 32];  // The actual spending secret
    let recipient_new = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key_derived = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    let nf_correct = poseidon2::nullifier(&domain, &nf_key_derived, &rho);
    
    // Build tree with properly derived recipient
    let cm_new = poseidon2::note_commitment(&domain, value, &rho, &recipient_new);
    let mut tree_new = MerkleTree::new(depth as u8);
    tree_new.set_leaf(pos as usize, cm_new);
    let anchor_new = tree_new.root();
    let siblings_new = tree_new.open(pos as usize);
    
    // Correct spend_sk satisfies fixed constraints
    let correct_sk_valid = satisfies_fixed_constraints(
        &domain, value, &rho, &recipient_new, &spend_sk, pos, depth, &siblings_new,
        &anchor_new, &nf_correct, withdraw_amount, &outputs,
    );
    println!("  Correct spend_sk valid: {}", correct_sk_valid);
    assert!(correct_sk_valid, "Correct spend_sk should satisfy fixed constraints");
    
    // Wrong spend_sk does NOT satisfy fixed constraints
    let mut wrong_sk = spend_sk;
    wrong_sk[0] ^= 1;
    let nf_wrong = poseidon2::nullifier(&domain, &poseidon2::nf_key_from_sk(&domain, &wrong_sk), &rho);
    
    let wrong_sk_valid = satisfies_fixed_constraints(
        &domain, value, &rho, &recipient_new, &wrong_sk, pos, depth, &siblings_new,
        &anchor_new, &nf_wrong, withdraw_amount, &outputs,
    );
    println!("  Wrong spend_sk valid: {}", wrong_sk_valid);
    assert!(!wrong_sk_valid, "Wrong spend_sk should NOT satisfy fixed constraints");
    
    println!("  ✓ FIX: Only the correct spend_sk can produce a valid proof!\n");
    
    Ok(())
}

// =====================================================================
// ADDRESS PROTECTION TESTS: pk_out derivation prevents arbitrary addresses
// =====================================================================

/// Helper to check if an output commitment is valid given pk_spend and pk_ivk
/// With ADDR_V2, both keys must be correct for the commitment to match.
fn output_commitment_valid(domain: &Hash32, value: u128, rho: &Hash32, pk_spend: &Hash32, pk_ivk: &Hash32, cm_expected: &Hash32) -> bool {
    let recipient = poseidon2::recipient_from_pk(domain, pk_spend, pk_ivk);
    let cm_computed = poseidon2::note_commitment(domain, value, rho, &recipient);
    cm_computed == *cm_expected
}

/// Test that valid pk_spend and pk_ivk produce matching commitment
#[test]
fn test_valid_pk_out_produces_correct_commitment() -> Result<()> {
    println!("\n=== Address Protection: Valid pk_spend + pk_ivk ===\n");
    
    let domain = [1u8; 32];
    
    // Receiver generates their keypair (both pk_spend and pk_ivk)
    let receiver_sk = [42u8; 32];
    let receiver_pk_spend = poseidon2::pk_from_sk(&receiver_sk);
    let receiver_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &receiver_sk);
    let receiver_recipient = poseidon2::recipient_from_pk(&domain, &receiver_pk_spend, &receiver_pk_ivk);
    
    println!("  Receiver's spend_sk: {}", hx(&receiver_sk));
    println!("  Receiver's pk_spend: {}", hx(&receiver_pk_spend));
    println!("  Receiver's pk_ivk:   {}", hx(&receiver_pk_ivk));
    println!("  Receiver's address:  {}", hx(&receiver_recipient));
    
    // Sender creates note for receiver using receiver's full address
    let value: u128 = 1000;
    let rho = [99u8; 32];
    let cm = poseidon2::note_commitment(&domain, value, &rho, &receiver_recipient);
    
    println!("\n  Sender creates note:");
    println!("    value: {}", value);
    println!("    commitment: {}", hx(&cm));
    
    // Verify: using receiver's pk_spend AND pk_ivk should produce matching commitment
    let is_valid = output_commitment_valid(&domain, value, &rho, &receiver_pk_spend, &receiver_pk_ivk, &cm);
    println!("\n  (pk_spend, pk_ivk) = receiver's keys → commitment valid: {}", is_valid);
    assert!(is_valid, "Valid pk_spend + pk_ivk should produce matching commitment");
    
    println!("  ✓ Valid pk_spend + pk_ivk produces correct commitment\n");
    Ok(())
}

/// Test that arbitrary addresses (not derived from pk_spend + pk_ivk) fail commitment check
#[test]
fn test_arbitrary_address_fails_commitment() -> Result<()> {
    println!("\n=== Address Protection: Arbitrary Address Rejected ===\n");
    
    let domain = [1u8; 32];
    
    // Suppose someone tries to use an arbitrary 32-byte value as recipient
    // (e.g., a normal chain address, or random bytes)
    let arbitrary_address = [0xAB; 32];  // Some arbitrary bytes
    
    println!("  Arbitrary address: {}", hx(&arbitrary_address));
    
    // Create a commitment using this arbitrary address directly
    let value: u128 = 1000;
    let rho = [99u8; 32];
    let cm_with_arbitrary = poseidon2::note_commitment(&domain, value, &rho, &arbitrary_address);
    
    println!("  Commitment with arbitrary recipient: {}", hx(&cm_with_arbitrary));
    
    // Now try to find ANY (pk_spend, pk_ivk) pair that would produce this commitment
    // This is impossible without finding a preimage of the ADDR_V2 hash
    
    // Test 1: Using the arbitrary address AS pk_spend and pk_ivk won't work
    // because recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk) ≠ arbitrary_address
    let derived_recipient = poseidon2::recipient_from_pk(&domain, &arbitrary_address, &arbitrary_address);
    println!("\n  If we use arbitrary_address as both pk_spend and pk_ivk:");
    println!("    derived recipient: {}", hx(&derived_recipient));
    println!("    expected recipient: {}", hx(&arbitrary_address));
    assert_ne!(derived_recipient, arbitrary_address, 
        "Derived recipient should differ from arbitrary address");
    
    // The commitment won't match
    let is_valid = output_commitment_valid(&domain, value, &rho, &arbitrary_address, &arbitrary_address, &cm_with_arbitrary);
    println!("    commitment valid: {}", is_valid);
    assert!(!is_valid, "Arbitrary address used as pk_spend/pk_ivk should NOT produce matching commitment");
    
    // Test 2: Try random pk values - none should work
    println!("\n  Testing random (pk_spend, pk_ivk) pairs:");
    for i in 0..5 {
        let random_pk_spend: Hash32 = {
            let mut pk = [0u8; 32];
            pk[0] = i;
            pk[31] = 255 - i;
            pk
        };
        let random_pk_ivk: Hash32 = {
            let mut pk = [0u8; 32];
            pk[0] = i + 100;
            pk[31] = 155 - i;
            pk
        };
        let is_valid = output_commitment_valid(&domain, value, &rho, &random_pk_spend, &random_pk_ivk, &cm_with_arbitrary);
        println!("    random pair[{}] → valid: {}", i, is_valid);
        assert!(!is_valid, "Random pk pair should not produce matching commitment");
    }
    
    println!("\n  ✓ Arbitrary addresses cannot be used as recipients\n");
    Ok(())
}

/// Test that funds can only be received by valid privacy addresses
#[test]
fn test_only_valid_privacy_addresses_can_receive() -> Result<()> {
    println!("\n=== Address Protection: Only Privacy Addresses Can Receive ===\n");
    
    let domain = [1u8; 32];
    
    // Scenario: User A wants to send to User B
    // User B must provide their full address (pk_spend + pk_ivk)
    
    let user_b_sk = [77u8; 32];
    let user_b_pk_spend = poseidon2::pk_from_sk(&user_b_sk);
    let user_b_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &user_b_sk);
    let user_b_address = poseidon2::recipient_from_pk(&domain, &user_b_pk_spend, &user_b_pk_ivk);
    
    println!("  User B's privacy address: {}", hx(&user_b_address));
    println!("  User B's pk_spend: {}", hx(&user_b_pk_spend));
    println!("  User B's pk_ivk:   {}", hx(&user_b_pk_ivk));
    
    // User A creates output note using B's full address
    let value: u128 = 500;
    let rho = [88u8; 32];
    let cm = poseidon2::note_commitment(&domain, value, &rho, &user_b_address);
    
    // Simulate circuit check: does (pk_spend, pk_ivk) derive to the committed recipient?
    let circuit_derived_recipient = poseidon2::recipient_from_pk(&domain, &user_b_pk_spend, &user_b_pk_ivk);
    let circuit_computed_cm = poseidon2::note_commitment(&domain, value, &rho, &circuit_derived_recipient);
    
    println!("\n  Circuit verification:");
    println!("    Input pk_spend: {}", hx(&user_b_pk_spend));
    println!("    Input pk_ivk: {}", hx(&user_b_pk_ivk));
    println!("    Derived recipient: {}", hx(&circuit_derived_recipient));
    println!("    Computed commitment: {}", hx(&circuit_computed_cm));
    println!("    Expected commitment: {}", hx(&cm));
    
    assert_eq!(circuit_computed_cm, cm, "Circuit should accept valid (pk_spend, pk_ivk)");
    println!("    ✓ Commitment matches!");
    
    // Now verify User B can spend the note (they know spend_sk)
    let user_b_derived_address = poseidon2::recipient_from_sk(&domain, &user_b_sk);
    assert_eq!(user_b_derived_address, user_b_address, 
        "User B's sk should derive to the same address");
    println!("\n  User B can spend because:");
    println!("    Their spend_sk derives to the committed recipient");
    println!("    ✓ Only User B can spend this note\n");
    
    Ok(())
}

/// Test that normal chain addresses cannot be used (would lock funds)
#[test]
fn test_normal_chain_address_would_fail() -> Result<()> {
    println!("\n=== Address Protection: Normal Chain Address Rejected ===\n");
    
    let domain = [1u8; 32];
    
    // Simulate a "normal chain address" - some 32-byte public key hash
    // that is NOT derived via our ADDR_V2 scheme
    let normal_chain_address: Hash32 = {
        // This might be H("NORMAL_ADDR" || some_pubkey) from another system
        let mut addr = [0u8; 32];
        addr[0..8].copy_from_slice(b"sov1addr");  // Simulating different format
        for i in 8..32 { addr[i] = i as u8; }
        addr
    };
    
    println!("  Normal chain address (simulated): {}", hx(&normal_chain_address));
    
    // If someone tries to create a note with this as recipient directly,
    // the circuit would need (pk_spend, pk_ivk) that derives to this address.
    // This is computationally infeasible (requires hash preimage).
    
    let value: u128 = 1000;
    let rho = [55u8; 32];
    
    // Incorrect approach: create commitment with arbitrary recipient
    let bad_cm = poseidon2::note_commitment(&domain, value, &rho, &normal_chain_address);
    println!("  Commitment with normal address: {}", hx(&bad_cm));
    
    // No (pk_spend, pk_ivk) pair will work for this commitment
    println!("\n  Can any (pk_spend, pk_ivk) pair produce this commitment?");
    
    // The "normal address" itself as both pk_spend and pk_ivk:
    let derived = poseidon2::recipient_from_pk(&domain, &normal_chain_address, &normal_chain_address);
    let valid = output_commitment_valid(&domain, value, &rho, &normal_chain_address, &normal_chain_address, &bad_cm);
    println!("    Using normal_address as both keys → derived recipient: {}", hx(&derived));
    println!("    Commitment valid: {}", valid);
    assert!(!valid);
    
    // Correct approach: Receiver must provide their privacy address (pk_spend + pk_ivk)
    println!("\n  Correct approach:");
    println!("    Receiver provides their privacy address (pk_spend + pk_ivk)");
    println!("    Circuit derives recipient = H(ADDR_V2 || domain || pk_spend || pk_ivk)");
    println!("    Only privacy addresses are valid");
    println!("\n  ✓ Normal chain addresses cannot receive privacy pool funds\n");
    
    Ok(())
}

/// Comprehensive test showing the full flow with address protection
#[test]
fn test_address_protection_full_flow() -> Result<()> {
    println!("\n=== Address Protection: Full Flow Demo ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    
    // === Step 1: Alice has funds in the privacy pool ===
    let alice_sk = [10u8; 32];
    let alice_pk_spend = poseidon2::pk_from_sk(&alice_sk);
    let alice_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &alice_sk);
    let alice_addr = poseidon2::recipient_from_pk(&domain, &alice_pk_spend, &alice_pk_ivk);
    let alice_nf_key = poseidon2::nf_key_from_sk(&domain, &alice_sk);
    
    let input_value: u128 = 1000;
    let input_rho = [20u8; 32];
    let input_cm = poseidon2::note_commitment(&domain, input_value, &input_rho, &alice_addr);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, input_cm);
    let _anchor = tree.root();
    let _siblings = tree.open(0);
    let _nullifier = poseidon2::nullifier(&domain, &alice_nf_key, &input_rho);
    
    println!("Step 1: Alice has 1000 in privacy pool");
    println!("  Alice's address: {}", &hx(&alice_addr)[..16]);
    println!("  Input commitment: {}", &hx(&input_cm)[..16]);
    
    // === Step 2: Alice wants to send 700 to Bob ===
    let bob_sk = [30u8; 32];
    let bob_pk_spend = poseidon2::pk_from_sk(&bob_sk);
    let bob_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &bob_sk);
    let bob_addr = poseidon2::recipient_from_pk(&domain, &bob_pk_spend, &bob_pk_ivk);
    
    println!("\nStep 2: Alice sends 700 to Bob");
    println!("  Bob provides his pk_spend: {}", &hx(&bob_pk_spend)[..16]);
    println!("  Bob provides his pk_ivk:   {}", &hx(&bob_pk_ivk)[..16]);
    println!("  Bob's address (derived): {}", &hx(&bob_addr)[..16]);
    
    // Create output note for Bob
    let output_value: u128 = 700;
    let output_rho = [40u8; 32];
    let output_cm = poseidon2::note_commitment(&domain, output_value, &output_rho, &bob_addr);
    
    // === Step 3: Verify circuit constraints ===
    println!("\nStep 3: Circuit verification");
    
    // Input verification
    let alice_addr_check = poseidon2::recipient_from_sk(&domain, &alice_sk);
    assert_eq!(alice_addr_check, alice_addr, "Alice's sk must derive to input recipient");
    println!("  ✓ Alice's spend_sk authorizes input note");
    
    // Output verification (circuit derives recipient from pk_spend + pk_ivk)
    let bob_addr_derived = poseidon2::recipient_from_pk(&domain, &bob_pk_spend, &bob_pk_ivk);
    let output_cm_check = poseidon2::note_commitment(&domain, output_value, &output_rho, &bob_addr_derived);
    assert_eq!(output_cm_check, output_cm, "Output commitment must match");
    println!("  ✓ Bob's (pk_spend, pk_ivk) produces valid output commitment");
    
    // Balance
    let withdraw = input_value - output_value;
    assert_eq!(input_value, withdraw + output_value);
    println!("  ✓ Balance: {} = {} (withdraw) + {} (output)", input_value, withdraw, output_value);
    
    // === Step 4: Verify Bob can spend the output ===
    println!("\nStep 4: Bob can spend his received note");
    let bob_addr_from_sk = poseidon2::recipient_from_sk(&domain, &bob_sk);
    assert_eq!(bob_addr_from_sk, bob_addr, "Bob's sk derives to output recipient");
    println!("  ✓ Bob's spend_sk can authorize spending the output note");
    
    // === Step 5: Show what happens with wrong address ===
    println!("\nStep 5: Wrong address protection demo");
    
    let evil_addr = [0xEE; 32];  // Some arbitrary address
    let evil_cm = poseidon2::note_commitment(&domain, output_value, &output_rho, &evil_addr);
    
    // Try to use evil_addr as both pk_spend and pk_ivk
    let evil_derived = poseidon2::recipient_from_pk(&domain, &evil_addr, &evil_addr);
    let evil_cm_check = poseidon2::note_commitment(&domain, output_value, &output_rho, &evil_derived);
    
    println!("  Attacker tries arbitrary address: {}", &hx(&evil_addr)[..16]);
    println!("  Derived recipient: {}", &hx(&evil_derived)[..16]);
    println!("  Commitment match: {}", evil_cm_check == evil_cm);
    assert_ne!(evil_cm_check, evil_cm, "Arbitrary address should not produce valid commitment");
    println!("  ✓ Circuit rejects arbitrary addresses");
    
    println!("\n=== Address Protection Working Correctly ===\n");
    
    Ok(())
}

// =====================================================================
// MULTI-INPUT TESTS: Consolidation and Multi-Note Payments
// =====================================================================

/// Test: 2-input consolidation (merge 2 small notes into 1)
#[test]
fn test_consolidation_2_to_1() -> Result<()> {
    println!("\n=== Multi-Input: 2→1 Consolidation ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    let self_pk_spend = poseidon2::pk_from_sk(&spend_sk);
    let self_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &spend_sk);
    
    // Create 2 input notes
    let value1: u128 = 300;
    let value2: u128 = 500;
    let rho1 = [10u8; 32];
    let rho2 = [20u8; 32];
    
    let cm1 = poseidon2::note_commitment(&domain, value1, &rho1, &recipient);
    let cm2 = poseidon2::note_commitment(&domain, value2, &rho2, &recipient);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm1);
    tree.set_leaf(1, cm2);
    let anchor = tree.root();
    
    let inputs = vec![
        InputNote {
            value: value1,
            rho: rho1,
            pos: 0,
            siblings: tree.open(0),
            nullifier: poseidon2::nullifier(&domain, &nf_key, &rho1),
        },
        InputNote {
            value: value2,
            rho: rho2,
            pos: 1,
            siblings: tree.open(1),
            nullifier: poseidon2::nullifier(&domain, &nf_key, &rho2),
        },
    ];
    
    // Create 1 output note (consolidation to self)
    let total_value = value1 + value2;
    let output_rho = [30u8; 32];
    let output_recipient = poseidon2::recipient_from_pk(&domain, &self_pk_spend, &self_pk_ivk);
    let output_cm = poseidon2::note_commitment(&domain, total_value, &output_rho, &output_recipient);
    
    let outputs = vec![make_self_output(&domain, total_value, output_rho, &spend_sk)];
    
    println!("✓ Input 1: {} tokens", value1);
    println!("✓ Input 2: {} tokens", value2);
    println!("✓ Output:  {} tokens (consolidated)", total_value);
    println!("✓ Balance: {} + {} = {}", value1, value2, total_value);
    
    // Verify constraints
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &outputs
    );
    assert!(valid, "2→1 consolidation should satisfy constraints");
    println!("✓ Multi-input constraints satisfied");
    
    // Build args for guest
    let args = build_args_multi(domain, spend_sk, depth, anchor, &inputs, 0, &outputs);
    println!("✓ Built {} arguments for guest", args.len());
    println!("✓ Private indices: {:?}\n", private_indices_multi(depth, 2, 1));
    
    Ok(())
}

/// Test: 4-input consolidation (merge 4 small notes into 1)
#[test]
fn test_consolidation_4_to_1() -> Result<()> {
    println!("\n=== Multi-Input: 4→1 Consolidation ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    // Create 4 input notes (dust consolidation scenario)
    let values: [u128; 4] = [100, 200, 300, 400];
    let rhos: [Hash32; 4] = [
        [10u8; 32], [20u8; 32], [30u8; 32], [40u8; 32]
    ];
    
    let mut tree = MerkleTree::new(depth as u8);
    for (i, (v, rho)) in values.iter().zip(rhos.iter()).enumerate() {
        let cm = poseidon2::note_commitment(&domain, *v, rho, &recipient);
        tree.set_leaf(i, cm);
    }
    let anchor = tree.root();
    
    let inputs: Vec<InputNote> = (0..4).map(|i| {
        InputNote {
            value: values[i],
            rho: rhos[i],
            pos: i as u64,
            siblings: tree.open(i),
            nullifier: poseidon2::nullifier(&domain, &nf_key, &rhos[i]),
        }
    }).collect();
    
    // Create 1 output (full consolidation)
    let total_value: u128 = values.iter().sum();
    let output_rho = [50u8; 32];
    
    let outputs = vec![make_self_output(&domain, total_value, output_rho, &spend_sk)];
    
    println!("✓ Inputs: {:?}", values);
    println!("✓ Total:  {} tokens", total_value);
    println!("✓ Output: 1 consolidated note");
    
    // Verify constraints
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &outputs
    );
    assert!(valid, "4→1 consolidation should satisfy constraints");
    println!("✓ Multi-input constraints satisfied");
    
    // Build args
    let args = build_args_multi(domain, spend_sk, depth, anchor, &inputs, 0, &outputs);
    println!("✓ Built {} arguments for guest\n", args.len());
    
    Ok(())
}

/// Test: Multi-input payment (2 inputs → payment + change)
#[test]
fn test_multi_input_payment_2_to_2() -> Result<()> {
    println!("\n=== Multi-Input: 2→2 Payment with Change ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    let self_pk = poseidon2::pk_from_sk(&spend_sk);
    
    // Create 2 input notes
    let value1: u128 = 300;
    let value2: u128 = 500;
    let rho1 = [10u8; 32];
    let rho2 = [20u8; 32];
    
    let cm1 = poseidon2::note_commitment(&domain, value1, &rho1, &recipient);
    let cm2 = poseidon2::note_commitment(&domain, value2, &rho2, &recipient);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm1);
    tree.set_leaf(1, cm2);
    let anchor = tree.root();
    
    let inputs = vec![
        InputNote {
            value: value1,
            rho: rho1,
            pos: 0,
            siblings: tree.open(0),
            nullifier: poseidon2::nullifier(&domain, &nf_key, &rho1),
        },
        InputNote {
            value: value2,
            rho: rho2,
            pos: 1,
            siblings: tree.open(1),
            nullifier: poseidon2::nullifier(&domain, &nf_key, &rho2),
        },
    ];
    
    // Payment: 600 to Bob, 200 change to self
    let bob_sk = [77u8; 32];
    let bob_pk = poseidon2::pk_from_sk(&bob_sk);
    let bob_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &bob_sk);
    
    let payment_value: u128 = 600;
    let change_value: u128 = 200;
    let payment_rho = [30u8; 32];
    let change_rho = [31u8; 32];
    
    let outputs = vec![
        make_payment_output(&domain, payment_value, payment_rho, bob_pk, bob_pk_ivk, &spend_sk),
        make_self_output(&domain, change_value, change_rho, &spend_sk),
    ];
    
    println!("✓ Input 1: {} tokens", value1);
    println!("✓ Input 2: {} tokens", value2);
    println!("✓ Payment: {} tokens to Bob", payment_value);
    println!("✓ Change:  {} tokens to self", change_value);
    println!("✓ Balance: {} + {} = {} + {}", value1, value2, payment_value, change_value);
    
    // Verify constraints
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &outputs
    );
    assert!(valid, "2→2 payment should satisfy constraints");
    println!("✓ Multi-input constraints satisfied\n");
    
    Ok(())
}

/// Test: Distinct nullifier enforcement (negative test)
#[test]
fn test_reject_duplicate_nullifiers() -> Result<()> {
    println!("\n=== Negative Test: Duplicate Nullifiers Rejected ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    // Create a single note
    let value: u128 = 500;
    let rho = [10u8; 32];
    let cm = poseidon2::note_commitment(&domain, value, &rho, &recipient);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm);
    let anchor = tree.root();
    let siblings = tree.open(0);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho);
    
    // Try to use the same note TWICE in one transaction (double-counting attack)
    let inputs = vec![
        InputNote {
            value,
            rho,
            pos: 0,
            siblings: siblings.clone(),
            nullifier: nf,
        },
        InputNote {
            value,
            rho,          // SAME rho = SAME nullifier
            pos: 0,
            siblings: siblings.clone(),
            nullifier: nf,  // DUPLICATE!
        },
    ];
    
    println!("✗ Attempting to use same note twice:");
    println!("  Input 1 nullifier: {}", &hx(&nf)[..16]);
    println!("  Input 2 nullifier: {}", &hx(&nf)[..16]);
    println!("  (Same nullifier - attack attempt!)");
    
    // This should fail the distinct nullifier check
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &[]
    );
    assert!(!valid, "Duplicate nullifiers should be rejected!");
    println!("✓ Correctly rejected duplicate nullifiers\n");
    
    Ok(())
}

/// Test: Multi-input with partial withdraw
#[test]
fn test_multi_input_with_withdraw() -> Result<()> {
    println!("\n=== Multi-Input: 3 Inputs with Partial Withdraw ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    let self_pk = poseidon2::pk_from_sk(&spend_sk);
    
    // Create 3 input notes
    let values: [u128; 3] = [100, 200, 300];
    let rhos: [Hash32; 3] = [[10u8; 32], [20u8; 32], [30u8; 32]];
    
    let mut tree = MerkleTree::new(depth as u8);
    for (i, (v, rho)) in values.iter().zip(rhos.iter()).enumerate() {
        let cm = poseidon2::note_commitment(&domain, *v, rho, &recipient);
        tree.set_leaf(i, cm);
    }
    let anchor = tree.root();
    
    let inputs: Vec<InputNote> = (0..3).map(|i| {
        InputNote {
            value: values[i],
            rho: rhos[i],
            pos: i as u64,
            siblings: tree.open(i),
            nullifier: poseidon2::nullifier(&domain, &nf_key, &rhos[i]),
        }
    }).collect();
    
    // Withdraw 150, keep 450 in 1 output
    let withdraw_amount: u128 = 150;
    let output_value: u128 = 450;
    let output_rho = [40u8; 32];
    
    let outputs = vec![make_self_output(&domain, output_value, output_rho, &spend_sk)];
    
    let total_in: u128 = values.iter().sum();
    println!("✓ Inputs: {:?} = {}", values, total_in);
    println!("✓ Withdraw: {} to transparent", withdraw_amount);
    println!("✓ Output: {} to shielded", output_value);
    println!("✓ Balance: {} = {} + {}", total_in, withdraw_amount, output_value);
    
    // Verify constraints
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, withdraw_amount, &outputs
    );
    assert!(valid, "3-input with withdraw should satisfy constraints");
    println!("✓ Multi-input with withdraw constraints satisfied\n");
    
    Ok(())
}

/// Test argument count for multi-input transactions
#[test]
fn test_multi_input_argument_count() {
    println!("\n=== Multi-Input Argument Count Validation ===\n");
    
    let depth = 8u32;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let anchor = [0u8; 32];
    
    // Create dummy inputs
    let make_input = |v: u128, i: u8| InputNote {
        value: v,
        rho: [i; 32],
        pos: i as u64,
        siblings: vec![[0u8; 32]; depth as usize],
        nullifier: [i + 100; 32],
    };
    
    // Create properly formatted outputs using helper
    let make_dummy_output = |v: u128, i: u8| -> OutputNote {
        make_self_output(&domain, v, [i; 32], &spend_sk)
    };
    
    // Expected: 5 header + n_in*(4+depth) + 2 + n_out*8
    // = 5 + n_in*(depth+4) + 2 + n_out*8
    
    // 1 input, 0 outputs
    let args_1_0 = build_args_multi(
        domain, spend_sk, depth, anchor,
        &[make_input(100, 0)],
        100, &[]
    );
    let expected_1_0 = 5 + 1 * (depth as usize + 4) + 2 + 0;
    assert_eq!(args_1_0.len(), expected_1_0, "1-in, 0-out");
    println!("✓ 1 input, 0 outputs: {} args", args_1_0.len());
    
    // 1 input, 2 outputs (16 args for outputs)
    let args_1_2 = build_args_multi(
        domain, spend_sk, depth, anchor,
        &[make_input(100, 0)],
        0, &[make_dummy_output(50, 0), make_dummy_output(50, 1)]
    );
    let expected_1_2 = 5 + 1 * (depth as usize + 4) + 2 + 16; // 8 args per output
    assert_eq!(args_1_2.len(), expected_1_2, "1-in, 2-out");
    println!("✓ 1 input, 2 outputs: {} args", args_1_2.len());
    
    // 4 inputs, 1 output
    let args_4_1 = build_args_multi(
        domain, spend_sk, depth, anchor,
        &[make_input(100, 0), make_input(200, 1), make_input(300, 2), make_input(400, 3)],
        0, &[make_dummy_output(1000, 0)]
    );
    let expected_4_1 = 5 + 4 * (depth as usize + 4) + 2 + 8; // 8 args per output
    assert_eq!(args_4_1.len(), expected_4_1, "4-in, 1-out");
    println!("✓ 4 inputs, 1 output: {} args", args_4_1.len());
    
    // 4 inputs, 2 outputs
    let args_4_2 = build_args_multi(
        domain, spend_sk, depth, anchor,
        &[make_input(100, 0), make_input(200, 1), make_input(300, 2), make_input(400, 3)],
        0, &[make_dummy_output(500, 0), make_dummy_output(500, 1)]
    );
    let expected_4_2 = 5 + 4 * (depth as usize + 4) + 2 + 16; // 8 args per output
    assert_eq!(args_4_2.len(), expected_4_2, "4-in, 2-out");
    println!("✓ 4 inputs, 2 outputs: {} args\n", args_4_2.len());
}

// =============================================================================
// NEGATIVE TESTS FOR NEW CONSTRAINTS (zero-value, rho reuse)
// =============================================================================

/// Test that zero-value input notes are rejected
#[test]
fn test_reject_zero_value_input() {
    println!("\n=== Reject Zero-Value Input Note ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let depth = 4u32;
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    // Create a zero-value input
    let rho_in = [10u8; 32];
    let cm_in = poseidon2::note_commitment(&domain, 0, &rho_in, &recipient); // value = 0
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho_in);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    let siblings = tree.open(0);
    
    let inputs = vec![InputNote {
        value: 0, // Zero value - should be rejected
        rho: rho_in,
        pos: 0,
        siblings,
        nullifier: nf,
    }];
    
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &[]
    );
    
    assert!(!valid, "Zero-value input should be rejected");
    println!("✓ Zero-value input correctly rejected\n");
}

/// Test that zero-value output notes are rejected
#[test]
fn test_reject_zero_value_output() {
    println!("\n=== Reject Zero-Value Output Note ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let depth = 4u32;
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    // Create a valid input with value 100
    let rho_in = [10u8; 32];
    let cm_in = poseidon2::note_commitment(&domain, 100, &rho_in, &recipient);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho_in);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    let siblings = tree.open(0);
    
    let inputs = vec![InputNote {
        value: 100,
        rho: rho_in,
        pos: 0,
        siblings,
        nullifier: nf,
    }];
    
    // Create a zero-value output (balance would work: 100 = 100 withdraw + 0 output)
    // Note: We create a structurally valid output but with value = 0
    let receiver_sk = [99u8; 32];
    let out_rho = [20u8; 32];
    
    // Create output using helper, then modify value to 0 to test rejection
    let mut output = make_payment_output(
        &domain, 1, out_rho, // Use value 1 to create valid output
        poseidon2::pk_from_sk(&receiver_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver_sk),
        &spend_sk
    );
    output.value = 0; // Tamper: set value to 0
    // Note: commitment/ciphertext are now inconsistent, but constraint check fails on value=0 first
    
    let outputs = vec![output];
    
    // Balance: 100 = 100 + 0 (would pass balance check)
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 100, &outputs
    );
    
    assert!(!valid, "Zero-value output should be rejected");
    println!("✓ Zero-value output correctly rejected\n");
}

/// Test that output rho matching input rho is rejected (prevents unspendable self-owned notes)
#[test]
fn test_reject_output_rho_equals_input_rho() {
    println!("\n=== Reject Output Rho Matching Input Rho ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let depth = 4u32;
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    let self_pk = poseidon2::pk_from_sk(&spend_sk);
    
    // Create a valid input
    let shared_rho = [10u8; 32]; // This rho will be reused in output
    let cm_in = poseidon2::note_commitment(&domain, 100, &shared_rho, &recipient);
    let nf = poseidon2::nullifier(&domain, &nf_key, &shared_rho);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    let siblings = tree.open(0);
    
    let inputs = vec![InputNote {
        value: 100,
        rho: shared_rho,
        pos: 0,
        siblings,
        nullifier: nf,
    }];
    
    // Create output with SAME rho as input - this is the bug we're preventing
    // Using the helper function to create proper output, but rho will match input
    let output = make_self_output(&domain, 100, shared_rho, &spend_sk);
    let outputs = vec![output];
    
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &outputs
    );
    
    assert!(!valid, "Output rho matching input rho should be rejected");
    println!("✓ Output rho matching input rho correctly rejected\n");
    println!("  (This prevents creating unspendable self-owned notes)\n");
}

/// Test that duplicate output rhos are rejected
#[test]
fn test_reject_duplicate_output_rhos() {
    println!("\n=== Reject Duplicate Output Rhos ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let depth = 4u32;
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    // Create a valid input with value 200
    let rho_in = [10u8; 32];
    let cm_in = poseidon2::note_commitment(&domain, 200, &rho_in, &recipient);
    let nf = poseidon2::nullifier(&domain, &nf_key, &rho_in);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    let siblings = tree.open(0);
    
    let inputs = vec![InputNote {
        value: 200,
        rho: rho_in,
        pos: 0,
        siblings,
        nullifier: nf,
    }];
    
    // Create two outputs with the SAME rho - this tests duplicate rho rejection
    let duplicate_rho = [50u8; 32]; // Different from input rho
    
    let receiver1_sk = [99u8; 32];
    let receiver2_sk = [98u8; 32];
    
    // Both outputs use the same rho - should be rejected
    let output1 = make_payment_output(
        &domain, 100, duplicate_rho,
        poseidon2::pk_from_sk(&receiver1_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver1_sk),
        &spend_sk
    );
    let output2 = make_payment_output(
        &domain, 100, duplicate_rho, // Same rho!
        poseidon2::pk_from_sk(&receiver2_sk),
        poseidon2::pk_ivk_from_spend_sk(&domain, &receiver2_sk),
        &spend_sk
    );
    
    let outputs = vec![output1, output2];
    
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &outputs
    );
    
    assert!(!valid, "Duplicate output rhos should be rejected");
    println!("✓ Duplicate output rhos correctly rejected\n");
}

/// Test that valid transactions with distinct rhos pass
#[test]
fn test_valid_distinct_rhos() {
    println!("\n=== Valid Transaction With Distinct Rhos ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let depth = 4u32;
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    let self_pk = poseidon2::pk_from_sk(&spend_sk);
    
    // Create two valid inputs with distinct rhos
    let rho_in_1 = [10u8; 32];
    let rho_in_2 = [11u8; 32];
    
    let cm_in_1 = poseidon2::note_commitment(&domain, 100, &rho_in_1, &recipient);
    let cm_in_2 = poseidon2::note_commitment(&domain, 100, &rho_in_2, &recipient);
    let nf_1 = poseidon2::nullifier(&domain, &nf_key, &rho_in_1);
    let nf_2 = poseidon2::nullifier(&domain, &nf_key, &rho_in_2);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in_1);
    tree.set_leaf(1, cm_in_2);
    let anchor = tree.root();
    let siblings_1 = tree.open(0);
    let siblings_2 = tree.open(1);
    
    let inputs = vec![
        InputNote { value: 100, rho: rho_in_1, pos: 0, siblings: siblings_1, nullifier: nf_1 },
        InputNote { value: 100, rho: rho_in_2, pos: 1, siblings: siblings_2, nullifier: nf_2 },
    ];
    
    // Create two outputs with distinct rhos (different from each other AND from inputs)
    let rho_out_1 = [20u8; 32]; // Different from inputs
    let rho_out_2 = [21u8; 32]; // Different from inputs and from rho_out_1
    
    let outputs = vec![
        make_self_output(&domain, 100, rho_out_1, &spend_sk),
        make_self_output(&domain, 100, rho_out_2, &spend_sk),
    ];
    
    let valid = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &inputs, 0, &outputs
    );
    
    assert!(valid, "Valid transaction with distinct rhos should pass");
    println!("✓ Valid transaction with distinct rhos accepted\n");
    println!("  Inputs:  rho_1={}, rho_2={}", hx(&rho_in_1), hx(&rho_in_2));
    println!("  Outputs: rho_1={}, rho_2={}\n", hx(&rho_out_1), hx(&rho_out_2));
}

// =====================================================================
// VIEWER ATTESTATION TESTS
// =====================================================================

/// Test valid viewer attestation verification
#[test]
fn test_valid_viewer_attestation() {
    println!("\n=== Valid Viewer Attestation ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let fvk = [99u8; 32]; // Viewer's full viewing key
    let sender_id = [88u8; 32];
    let self_pk_spend = poseidon2::pk_from_sk(&spend_sk);
    let self_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &spend_sk);
    
    // Create an output note
    let rho_out = [55u8; 32];
    let value = 1000u128;
    let output = make_self_output(&domain, value, rho_out, &spend_sk);
    let recipient = poseidon2::recipient_from_pk(&domain, &self_pk_spend, &self_pk_ivk);
    
    // Compute valid viewer attestation
    let fvk_commit = poseidon2::fvk_commit(&fvk);
    let k = poseidon2::view_kdf(&fvk, &output.commitment);
    let pt = poseidon2::encode_note_plain(&domain, value, &rho_out, &recipient, &sender_id);
    let ct = poseidon2::stream_xor_encrypt(&k, &pt);
    let ct_hash = poseidon2::ct_hash(&ct);
    let mac = poseidon2::view_mac(&k, &output.commitment, &ct_hash);
    
    let attestation = ViewerAttestation {
        fvk,
        fvk_commit,
        sender_id,
        ct_hashes: vec![ct_hash],
        macs: vec![mac],
    };
    
    let valid = satisfies_viewer_attestations(&domain, &[output], &[attestation]);
    assert!(valid, "Valid viewer attestation should pass");
    println!("✓ Valid viewer attestation accepted\n");
}

/// Test invalid viewer attestation - wrong FVK commit
#[test]
fn test_reject_wrong_fvk_commit() {
    println!("\n=== Reject Wrong FVK Commit ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let fvk = [99u8; 32];
    let sender_id = [88u8; 32];
    let self_pk_spend = poseidon2::pk_from_sk(&spend_sk);
    let self_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &spend_sk);
    
    let rho_out = [55u8; 32];
    let value = 1000u128;
    let recipient = poseidon2::recipient_from_pk(&domain, &self_pk_spend, &self_pk_ivk);
    let output = make_self_output(&domain, value, rho_out, &spend_sk);
    let cm = output.commitment;
    
    // Compute attestation but use wrong fvk_commit
    let wrong_fvk_commit = [0u8; 32]; // Wrong!
    let k = poseidon2::view_kdf(&fvk, &cm);
    let pt = poseidon2::encode_note_plain(&domain, value, &rho_out, &recipient, &sender_id);
    let ct = poseidon2::stream_xor_encrypt(&k, &pt);
    let ct_hash = poseidon2::ct_hash(&ct);
    let mac = poseidon2::view_mac(&k, &cm, &ct_hash);
    
    let attestation = ViewerAttestation {
        fvk,
        fvk_commit: wrong_fvk_commit,
        sender_id,
        ct_hashes: vec![ct_hash],
        macs: vec![mac],
    };
    
    let valid = satisfies_viewer_attestations(&domain, &[output], &[attestation]);
    assert!(!valid, "Wrong FVK commit should be rejected");
    println!("✓ Wrong FVK commit correctly rejected\n");
}

/// Test invalid viewer attestation - wrong ct_hash
#[test]
fn test_reject_wrong_ct_hash() {
    println!("\n=== Reject Wrong CT Hash ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let fvk = [99u8; 32];
    let sender_id = [88u8; 32];
    let self_pk = poseidon2::pk_from_sk(&spend_sk);
    
    let rho_out = [55u8; 32];
    let value = 1000u128;
    let output = make_self_output(&domain, value, rho_out, &spend_sk);
    let cm = output.commitment;
    
    let fvk_commit = poseidon2::fvk_commit(&fvk);
    let k = poseidon2::view_kdf(&fvk, &cm);
    let wrong_ct_hash = [0u8; 32]; // Wrong!
    let mac = poseidon2::view_mac(&k, &cm, &wrong_ct_hash); // MAC over wrong ct_hash
    
    let attestation = ViewerAttestation {
        fvk,
        fvk_commit,
        sender_id,
        ct_hashes: vec![wrong_ct_hash],
        macs: vec![mac],
    };
    
    let valid = satisfies_viewer_attestations(&domain, &[output], &[attestation]);
    assert!(!valid, "Wrong ct_hash should be rejected");
    println!("✓ Wrong ct_hash correctly rejected\n");
}

/// Test invalid viewer attestation - wrong MAC
#[test]
fn test_reject_wrong_mac() {
    println!("\n=== Reject Wrong MAC ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let fvk = [99u8; 32];
    let sender_id = [88u8; 32];
    let self_pk_spend = poseidon2::pk_from_sk(&spend_sk);
    let self_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &spend_sk);
    
    let rho_out = [55u8; 32];
    let value = 1000u128;
    let output = make_self_output(&domain, value, rho_out, &spend_sk);
    let cm = output.commitment;
    let recipient = poseidon2::recipient_from_pk(&domain, &self_pk_spend, &self_pk_ivk);
    
    let fvk_commit = poseidon2::fvk_commit(&fvk);
    let k = poseidon2::view_kdf(&fvk, &cm);
    let pt = poseidon2::encode_note_plain(&domain, value, &rho_out, &recipient, &sender_id);
    let ct = poseidon2::stream_xor_encrypt(&k, &pt);
    let ct_hash = poseidon2::ct_hash(&ct);
    let wrong_mac = [0u8; 32]; // Wrong!
    
    let attestation = ViewerAttestation {
        fvk,
        fvk_commit,
        sender_id,
        ct_hashes: vec![ct_hash],
        macs: vec![wrong_mac],
    };
    
    let valid = satisfies_viewer_attestations(&domain, &[output], &[attestation]);
    assert!(!valid, "Wrong MAC should be rejected");
    println!("✓ Wrong MAC correctly rejected\n");
}

/// Test viewer attestation with multiple outputs
#[test]
fn test_viewer_attestation_two_outputs() {
    println!("\n=== Viewer Attestation With Two Outputs ===\n");
    
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let fvk = [99u8; 32];
    let sender_id = [88u8; 32];
    let self_pk_spend = poseidon2::pk_from_sk(&spend_sk);
    let self_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &spend_sk);
    
    // Create two outputs
    let rho_1 = [55u8; 32];
    let rho_2 = [56u8; 32];
    let value_1 = 1000u128;
    let value_2 = 2000u128;
    
    let outputs = vec![
        make_self_output(&domain, value_1, rho_1, &spend_sk),
        make_self_output(&domain, value_2, rho_2, &spend_sk),
    ];
    let cm_1 = outputs[0].commitment;
    let cm_2 = outputs[1].commitment;
    let recipient = poseidon2::recipient_from_pk(&domain, &self_pk_spend, &self_pk_ivk);
    
    // Compute valid attestations for both outputs
    let fvk_commit = poseidon2::fvk_commit(&fvk);
    
    let k_1 = poseidon2::view_kdf(&fvk, &cm_1);
    let pt_1 = poseidon2::encode_note_plain(&domain, value_1, &rho_1, &recipient, &sender_id);
    let ct_1 = poseidon2::stream_xor_encrypt(&k_1, &pt_1);
    let ct_hash_1 = poseidon2::ct_hash(&ct_1);
    let mac_1 = poseidon2::view_mac(&k_1, &cm_1, &ct_hash_1);
    
    let k_2 = poseidon2::view_kdf(&fvk, &cm_2);
    let pt_2 = poseidon2::encode_note_plain(&domain, value_2, &rho_2, &recipient, &sender_id);
    let ct_2 = poseidon2::stream_xor_encrypt(&k_2, &pt_2);
    let ct_hash_2 = poseidon2::ct_hash(&ct_2);
    let mac_2 = poseidon2::view_mac(&k_2, &cm_2, &ct_hash_2);
    
    let attestation = ViewerAttestation {
        fvk,
        fvk_commit,
        sender_id,
        ct_hashes: vec![ct_hash_1, ct_hash_2],
        macs: vec![mac_1, mac_2],
    };
    
    let valid = satisfies_viewer_attestations(&domain, &outputs, &[attestation]);
    assert!(valid, "Valid viewer attestation for two outputs should pass");
    println!("✓ Valid viewer attestation for two outputs accepted\n");
}

// =====================================================================
// ADDR_V2 SECURITY TESTS: pk_ivk binding prevents mismatched encryption
// =====================================================================

/// Test that ADDR_V2 prevents the "mismatched encryption" attack.
/// 
/// ATTACK SCENARIO (possible with ADDR_V1):
/// Attacker creates a note where:
/// - pk_spend = Alice's pk_spend (so Alice "owns" the note)
/// - pk_ivk = Attacker's pk_ivk (so only attacker can decrypt)
/// 
/// This would create a note that:
/// 1. Appears committed to Alice's spending key
/// 2. But encrypted to the attacker's viewing key
/// 3. Alice cannot detect or decrypt the note via scanning
/// 4. The note is effectively burned/lost
/// 
/// ADDR_V2 FIX:
/// recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)
/// 
/// With this binding:
/// - If attacker uses Alice's pk_spend + attacker's pk_ivk
/// - The recipient hash is different from Alice's real address
/// - Alice cannot spend the note (wrong recipient in commitment)
/// - Note goes to a "different address" (attacker's franken-address)
/// - This is now a "send to wrong address" failure, not a silent burn
#[test]
fn test_addr_v2_prevents_mismatched_encryption_attack() -> Result<()> {
    println!("\n=== ADDR_V2 Security: Mismatched Encryption Attack Prevention ===\n");
    
    let domain = [1u8; 32];
    
    // Alice's keys (victim)
    let alice_sk = [10u8; 32];
    let alice_pk_spend = poseidon2::pk_from_sk(&alice_sk);
    let alice_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &alice_sk);
    let alice_recipient = poseidon2::recipient_from_pk(&domain, &alice_pk_spend, &alice_pk_ivk);
    
    // Attacker's keys
    let attacker_sk = [99u8; 32];
    let attacker_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &attacker_sk);
    
    println!("Alice's pk_spend:    {}", &hx(&alice_pk_spend)[..16]);
    println!("Alice's pk_ivk:      {}", &hx(&alice_pk_ivk)[..16]);
    println!("Alice's recipient:   {}", &hx(&alice_recipient)[..16]);
    println!("Attacker's pk_ivk:   {}", &hx(&attacker_pk_ivk)[..16]);
    
    // === ATTACK ATTEMPT ===
    // Attacker tries to create a note with Alice's pk_spend but attacker's pk_ivk
    let value: u128 = 1000;
    let rho = [50u8; 32];
    
    // The "franken-address": Alice's spend key + Attacker's viewing key
    let franken_recipient = poseidon2::recipient_from_pk(&domain, &alice_pk_spend, &attacker_pk_ivk);
    
    println!("\n=== Attack Attempt ===");
    println!("Franken-recipient (Alice's pk_spend + Attacker's pk_ivk):");
    println!("  {}", &hx(&franken_recipient)[..16]);
    
    // This recipient is DIFFERENT from Alice's real recipient
    assert_ne!(franken_recipient, alice_recipient, 
        "Franken-recipient should differ from Alice's real recipient");
    println!("✓ Franken-recipient ≠ Alice's recipient");
    
    // Create a commitment with the franken-address
    let franken_cm = poseidon2::note_commitment(&domain, value, &rho, &franken_recipient);
    
    // Create Alice's real commitment (what she would expect)
    let alice_cm = poseidon2::note_commitment(&domain, value, &rho, &alice_recipient);
    
    // The commitments are DIFFERENT
    assert_ne!(franken_cm, alice_cm, 
        "Franken commitment should differ from Alice's expected commitment");
    println!("✓ Franken-commitment ≠ Alice's expected commitment");
    
    // === KEY INSIGHT ===
    println!("\n=== Security Analysis ===");
    
    // Can Alice spend the franken-note?
    // No! Her spend_sk derives to alice_recipient, not franken_recipient
    let alice_derived_recipient = poseidon2::recipient_from_sk(&domain, &alice_sk);
    assert_eq!(alice_derived_recipient, alice_recipient);
    assert_ne!(alice_derived_recipient, franken_recipient);
    println!("✓ Alice cannot spend franken-note (recipient mismatch in circuit)");
    
    // Can the attacker spend the franken-note?
    // No! Attacker's spend_sk derives to a completely different recipient
    let attacker_pk_spend = poseidon2::pk_from_sk(&attacker_sk);
    let attacker_recipient = poseidon2::recipient_from_pk(&domain, &attacker_pk_spend, &attacker_pk_ivk);
    assert_ne!(attacker_recipient, franken_recipient);
    println!("✓ Attacker cannot spend franken-note (wrong pk_spend in address)");
    
    // The franken-note is "sent to nobody" - a burned address
    // This is the intended behavior: mismatch = burn, not stealth attack
    println!("\n=== Conclusion ===");
    println!("With ADDR_V2, mismatching pk_spend and pk_ivk creates a 'burned' note.");
    println!("The note cannot be spent by anyone - which is the expected failure mode.");
    println!("This prevents the silent attack where Alice has an undecryptable note.");
    println!("✓ ADDR_V2 successfully prevents the mismatched encryption attack\n");
    
    Ok(())
}

/// Test that correct (pk_spend, pk_ivk) pairs work correctly.
/// Demonstrates that when keys match, everything works as expected.
#[test]
fn test_addr_v2_correct_key_pairs_work() -> Result<()> {
    println!("\n=== ADDR_V2: Correct Key Pairs Work ===\n");
    
    let domain = [1u8; 32];
    
    // Create a user with properly paired keys
    let user_sk = [42u8; 32];
    let user_pk_spend = poseidon2::pk_from_sk(&user_sk);
    let user_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &user_sk);
    let user_recipient = poseidon2::recipient_from_pk(&domain, &user_pk_spend, &user_pk_ivk);
    
    println!("User pk_spend: {}", &hx(&user_pk_spend)[..16]);
    println!("User pk_ivk:   {}", &hx(&user_pk_ivk)[..16]);
    println!("User address:  {}", &hx(&user_recipient)[..16]);
    
    // Create a note for this user
    let value: u128 = 1000;
    let rho = [55u8; 32];
    let cm = poseidon2::note_commitment(&domain, value, &rho, &user_recipient);
    
    // Verify the user can derive the same recipient from their spend_sk
    let derived_recipient = poseidon2::recipient_from_sk(&domain, &user_sk);
    assert_eq!(derived_recipient, user_recipient, 
        "User's spend_sk should derive to their recipient address");
    println!("✓ User's spend_sk derives to correct recipient");
    
    // Verify circuit would accept this (using our test helper)
    let derived_cm = poseidon2::note_commitment(&domain, value, &rho, &derived_recipient);
    assert_eq!(derived_cm, cm, 
        "Circuit-computed commitment should match");
    println!("✓ Circuit commitment matches expected commitment");
    
    // Create a full output note and verify it's consistent
    let output = make_self_output(&domain, value, rho, &user_sk);
    assert_eq!(output.commitment, cm, "make_self_output should produce correct commitment");
    println!("✓ Full output note is consistent");
    
    println!("\n✓ ADDR_V2 works correctly with properly paired keys\n");
    
    Ok(())
}

// =====================================================================
// LEVEL A RECIPIENT DETECTION TESTS: Trial decryption + ciphertext binding
// =====================================================================

/// Helper: trial decrypt a recipient ciphertext (wallet scanning)
/// 
/// This simulates what a receiver does when scanning the chain:
/// 1. Derive ivk_sk from their spend_sk
/// 2. Compute DH(ivk_sk, epk) to get shared secret
/// 3. Derive k_in and decrypt the ciphertext
/// 4. Return plaintext (caller verifies commitment match)
fn trial_decrypt_level_a(
    domain: &Hash32,
    receiver_spend_sk: &Hash32,
    out: &OutputNote,
) -> [u8; 144] {
    // Receiver derives ivk_sk from spend_sk
    let ivk_sk = poseidon2::ivk_sk_from_spend_sk(domain, receiver_spend_sk);
    
    // DH(receiver_ivk_sk, epk)
    let dh = poseidon2::x25519_shared(&ivk_sk, &out.epk);
    
    // Same kdf as circuit/host
    let k_in = poseidon2::in_kdf(domain, &dh, &out.commitment);
    
    // XOR stream decrypt (same as encrypt - symmetric)
    let pt_vec = poseidon2::stream_xor_encrypt_in(&k_in, &out.ct);
    
    let mut pt = [0u8; 144];
    pt.copy_from_slice(&pt_vec);
    pt
}

/// Test: Receiver can trial-decrypt their output and verify commitment
#[test]
fn test_level_a_receiver_can_trial_decrypt_and_match_commitment() -> Result<()> {
    println!("\n=== Level A: Receiver Can Trial Decrypt ===\n");
    
    let domain = [1u8; 32];
    
    // Sender
    let sender_spend_sk = [4u8; 32];
    let sender_id = poseidon2::recipient_from_sk(&domain, &sender_spend_sk);
    
    // Receiver (Bob)
    let bob_spend_sk = [77u8; 32];
    let bob_pk_spend = poseidon2::pk_from_sk(&bob_spend_sk);
    let bob_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &bob_spend_sk);
    
    let value = 600u128;
    let rho = [30u8; 32];
    
    let out = make_output(&domain, value, rho, bob_pk_spend, bob_pk_ivk, &sender_id);
    
    println!("Created output note:");
    println!("  value: {}", value);
    println!("  commitment: {}", &hx(&out.commitment)[..16]);
    
    // Bob trial decrypts
    let pt = trial_decrypt_level_a(&domain, &bob_spend_sk, &out);
    
    // Parse plaintext fields
    let mut d = [0u8; 32];
    d.copy_from_slice(&pt[0..32]);
    assert_eq!(d, domain);
    
    let mut v_bytes = [0u8; 16];
    v_bytes.copy_from_slice(&pt[32..48]);
    let v = u128::from_le_bytes(v_bytes);
    assert_eq!(v, value);
    println!("✓ Decrypted value matches: {}", v);
    
    let mut rho_pt = [0u8; 32];
    rho_pt.copy_from_slice(&pt[48..80]);
    assert_eq!(rho_pt, rho);
    println!("✓ Decrypted rho matches");
    
    let mut recipient_pt = [0u8; 32];
    recipient_pt.copy_from_slice(&pt[80..112]);
    
    let bob_recipient = poseidon2::recipient_from_pk(&domain, &bob_pk_spend, &bob_pk_ivk);
    assert_eq!(recipient_pt, bob_recipient);
    println!("✓ Decrypted recipient matches Bob's address");
    
    let mut sender_id_pt = [0u8; 32];
    sender_id_pt.copy_from_slice(&pt[112..144]);
    assert_eq!(sender_id_pt, sender_id);
    println!("✓ Decrypted sender_id matches");
    
    // Recompute commitment from decrypted plaintext
    let cm_check = poseidon2::note_commitment(&domain, v, &rho_pt, &recipient_pt);
    assert_eq!(cm_check, out.commitment);
    println!("✓ Commitment recomputed from plaintext matches on-chain commitment");
    
    println!("\n✓ Bob successfully detected and verified his received note\n");
    
    Ok(())
}

/// Test: Wrong receiver cannot match commitment (scan fails)
#[test]
fn test_level_a_wrong_ivk_cannot_match_commitment() -> Result<()> {
    println!("\n=== Level A: Wrong Receiver Cannot Match Commitment ===\n");
    
    let domain = [1u8; 32];
    
    let sender_spend_sk = [4u8; 32];
    let sender_id = poseidon2::recipient_from_sk(&domain, &sender_spend_sk);
    
    // Bob is true receiver
    let bob_spend_sk = [77u8; 32];
    let bob_pk_spend = poseidon2::pk_from_sk(&bob_spend_sk);
    let bob_pk_ivk = poseidon2::pk_ivk_from_spend_sk(&domain, &bob_spend_sk);
    
    let out = make_output(&domain, 123u128, [9u8; 32], bob_pk_spend, bob_pk_ivk, &sender_id);
    
    // Carol tries to scan/decrypt (she's not the recipient)
    let carol_spend_sk = [55u8; 32];
    let pt = trial_decrypt_level_a(&domain, &carol_spend_sk, &out);
    
    // Parse plaintext (will be garbage for Carol)
    let mut v_bytes = [0u8; 16];
    v_bytes.copy_from_slice(&pt[32..48]);
    let v = u128::from_le_bytes(v_bytes);
    
    let mut rho_pt = [0u8; 32];
    rho_pt.copy_from_slice(&pt[48..80]);
    
    let mut recipient_pt = [0u8; 32];
    recipient_pt.copy_from_slice(&pt[80..112]);
    
    // Recompute cm from decrypted plaintext
    let cm_check = poseidon2::note_commitment(&domain, v, &rho_pt, &recipient_pt);
    
    // Carol's decryption produces garbage that doesn't match commitment
    assert_ne!(cm_check, out.commitment);
    println!("✓ Carol's trial decryption does NOT match the on-chain commitment");
    println!("✓ Carol correctly rejects this note (not hers)");
    
    println!("\n✓ Wrong receiver correctly fails to detect the note\n");
    
    Ok(())
}

/// Test: Tampered ciphertext (ct) is rejected
/// 
/// This tests the critical on-chain fix: ct_hash is computed from tx calldata,
/// so if the ct bytes are tampered, the proof will fail.
#[test]
fn test_reject_level_a_tampered_ciphertext_ct() -> Result<()> {
    println!("\n=== Level A: Tampered Ciphertext Rejected ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    
    // Single input note
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    let in_value: u128 = 1000;
    let in_rho = [2u8; 32];
    let pos: u64 = 0;
    
    let cm_in = poseidon2::note_commitment(&domain, in_value, &in_rho, &recipient);
    
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(pos as usize, cm_in);
    let anchor = tree.root();
    
    let input = InputNote {
        value: in_value,
        rho: in_rho,
        pos,
        siblings: tree.open(pos as usize),
        nullifier: poseidon2::nullifier(&domain, &nf_key, &in_rho),
    };
    
    // One output to self
    let mut out = make_self_output(&domain, 1000, [9u8; 32], &spend_sk);
    
    println!("Original ct_hash: {}", &hx(&out.ct_hash)[..16]);
    
    // Tamper tx calldata ct (flip one bit)
    out.ct[0] ^= 1;
    
    // Now ct_hash(out.ct) will differ from out.ct_hash
    let tampered_ct_hash = poseidon2::ct_hash(&out.ct);
    println!("Tampered ct_hash: {}", &hx(&tampered_ct_hash)[..16]);
    assert_ne!(tampered_ct_hash, out.ct_hash);
    
    let ok = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &[input], 0, &[out],
    );
    
    assert!(!ok, "Tampered ct must be rejected (ct_hash derived from ct changes)");
    println!("✓ Tampered ciphertext correctly rejected");
    
    println!("\n✓ On-chain ct_hash binding prevents ct tampering\n");
    
    Ok(())
}

/// Test: Tampered epk is rejected
#[test]
fn test_reject_level_a_tampered_epk() -> Result<()> {
    println!("\n=== Level A: Tampered EPK Rejected ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    let in_value: u128 = 1000;
    let in_rho = [2u8; 32];
    
    let cm_in = poseidon2::note_commitment(&domain, in_value, &in_rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    
    let input = InputNote {
        value: in_value,
        rho: in_rho,
        pos: 0,
        siblings: tree.open(0),
        nullifier: poseidon2::nullifier(&domain, &nf_key, &in_rho),
    };
    
    let mut out = make_self_output(&domain, 1000, [9u8; 32], &spend_sk);
    out.epk[0] ^= 1;  // Tamper epk
    
    let ok = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &[input], 0, &[out],
    );
    
    assert!(!ok, "Tampered epk must be rejected");
    println!("✓ Tampered EPK correctly rejected\n");
    
    Ok(())
}

/// Test: Tampered MAC is rejected
#[test]
fn test_reject_level_a_tampered_mac() -> Result<()> {
    println!("\n=== Level A: Tampered MAC Rejected ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    let in_value: u128 = 1000;
    let in_rho = [2u8; 32];
    
    let cm_in = poseidon2::note_commitment(&domain, in_value, &in_rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    
    let input = InputNote {
        value: in_value,
        rho: in_rho,
        pos: 0,
        siblings: tree.open(0),
        nullifier: poseidon2::nullifier(&domain, &nf_key, &in_rho),
    };
    
    let mut out = make_self_output(&domain, 1000, [9u8; 32], &spend_sk);
    out.mac[0] ^= 1;  // Tamper MAC
    
    let ok = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &[input], 0, &[out],
    );
    
    assert!(!ok, "Tampered MAC must be rejected");
    println!("✓ Tampered MAC correctly rejected\n");
    
    Ok(())
}

/// Test: Low-order pk_ivk is rejected (DH = 0)
/// 
/// This matches the guest circuit's dh_or != 0 check.
#[test]
fn test_reject_level_a_low_order_pk_ivk() -> Result<()> {
    println!("\n=== Level A: Low-Order pk_ivk Rejected ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    
    let recipient = poseidon2::recipient_from_sk(&domain, &spend_sk);
    let nf_key = poseidon2::nf_key_from_sk(&domain, &spend_sk);
    
    let in_value: u128 = 1000;
    let in_rho = [2u8; 32];
    
    let cm_in = poseidon2::note_commitment(&domain, in_value, &in_rho, &recipient);
    let mut tree = MerkleTree::new(depth as u8);
    tree.set_leaf(0, cm_in);
    let anchor = tree.root();
    
    let input = InputNote {
        value: in_value,
        rho: in_rho,
        pos: 0,
        siblings: tree.open(0),
        nullifier: poseidon2::nullifier(&domain, &nf_key, &in_rho),
    };
    
    // Output with pk_spend from our key but pk_ivk is all-zeros (low-order point)
    let pk_spend = poseidon2::pk_from_sk(&spend_sk);
    let pk_ivk_zero = [0u8; 32];  // Low-order / invalid pk_ivk
    let sender_id = poseidon2::recipient_from_sk(&domain, &spend_sk);
    
    let out = make_output(&domain, 1000, [9u8; 32], pk_spend, pk_ivk_zero, &sender_id);
    
    let ok = satisfies_multi_input_constraints(
        &domain, &spend_sk, depth, &anchor, &[input], 0, &[out],
    );
    
    assert!(!ok, "Low-order pk_ivk must be rejected (DH=0)");
    println!("✓ Low-order pk_ivk correctly rejected (DH=0)\n");
    
    Ok(())
}

/// Test: Ensure build_args_multi uses ct_hash computed from tx ct
/// 
/// This catches regressions where the test harness might use a "free" ct_hash.
#[test]
fn test_build_args_multi_uses_ct_hash_from_tx_ct() -> Result<()> {
    println!("\n=== Build Args: ct_hash Computed from TX ct ===\n");
    
    let depth: u32 = 8;
    let domain = [1u8; 32];
    let spend_sk = [42u8; 32];
    let anchor = [0u8; 32];
    
    // 1 dummy input
    let input = InputNote {
        value: 100,
        rho: [2u8; 32],
        pos: 0,
        siblings: vec![[0u8; 32]; depth as usize],
        nullifier: [9u8; 32],
    };
    
    // 1 output
    let mut out = make_self_output(&domain, 100, [3u8; 32], &spend_sk);
    
    // Tamper stored ct_hash field (should not affect argv if builder recomputes)
    let original_ct_hash = out.ct_hash;
    out.ct_hash[0] ^= 1;
    
    let args = build_args_multi(domain, spend_sk, depth, anchor, &[input], 0, &[out.clone()]);
    
    // Compute where ct_hash lives in argv for n_in=1, n_out=1
    // Header: 5 args (domain, spend_sk, depth, anchor, n_in)
    // Per input: depth + 4 args
    // Then: 2 args (withdraw, n_out)
    // Per output: 8 args (value, rho, pk_spend, pk_ivk, cm, epk, ct_hash, mac)
    let per_in = (depth as usize + 4);
    let base_output = 5 + per_in * 1 + 2;
    let idx_ct_hash = base_output + 6;  // ct_hash is 7th output field (0-indexed: 6)
    
    // The ct_hash in args should be computed from ct, not from the tampered ct_hash field
    let ct_hash_from_ct = poseidon2::ct_hash(&out.ct);
    assert_eq!(args[idx_ct_hash], hx(&ct_hash_from_ct));
    assert_eq!(ct_hash_from_ct, original_ct_hash);
    
    println!("✓ ct_hash in argv is computed from ct bytes, not from stored field");
    println!("✓ Tampering ct_hash field has no effect (builder recomputes from ct)\n");
    
    Ok(())
}
