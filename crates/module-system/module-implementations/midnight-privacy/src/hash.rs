//! Domain-separated Poseidon2-Goldilocks hash functions for privacy-preserving operations.
//!
//! This module uses neo-ccs's Poseidon2-Goldilocks implementation, which is compatible
//! with the Nightstream RISC-V circuit. Using the same Poseidon2 implementation ensures
//! hash consistency between native Rust code and the ZK circuit.
//!
//! Domain separation is achieved by prepending unique u64 domain tags (as Goldilocks field
//! elements) to each input type, preventing cross-domain collisions and attacks.
//!
//! ## Hash32 <-> GlDigest
//!
//! The circuit works with `GlDigest = [Goldilocks; 4]` (four 64-bit field elements).
//! On-chain state uses `Hash32 = [u8; 32]`. The encoding is:
//! - Each `Goldilocks` element → 8 bytes little-endian
//! - 4 elements → 32 bytes total
//!
//! This bijection is implemented by `gldigest_to_hash32` and `hash32_to_gldigest`.

use std::fmt;
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use neo_ccs::crypto::poseidon2_goldilocks as p2;
use p3_field::{PrimeCharacteristicRing, PrimeField64};
use p3_goldilocks::Goldilocks;
use serde::{Deserialize, Serialize};

/// 32-byte hash output.
pub type Hash32 = [u8; 32];

/// Goldilocks digest: 4 Goldilocks field elements = 32 bytes.
pub type GlDigest = [Goldilocks; 4];

// === Hash32 <-> GlDigest conversion ===

/// Convert a GlDigest (4 x Goldilocks) to Hash32 (32 bytes, little-endian).
#[inline]
pub fn gldigest_to_hash32(digest: &GlDigest) -> Hash32 {
    let mut out = [0u8; 32];
    for (i, &elem) in digest.iter().enumerate() {
        let val: u64 = elem.as_canonical_u64();
        out[i * 8..(i + 1) * 8].copy_from_slice(&val.to_le_bytes());
    }
    out
}

/// Convert a Hash32 (32 bytes) to GlDigest (4 x Goldilocks, little-endian).
#[inline]
pub fn hash32_to_gldigest(h: &Hash32) -> GlDigest {
    let mut out = [Goldilocks::ZERO; 4];
    for i in 0..4 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&h[i * 8..(i + 1) * 8]);
        out[i] = Goldilocks::from_u64(u64::from_le_bytes(buf));
    }
    out
}

// === Domain tags (must match the RISC-V circuit exactly) ===

const TAG_MT_NODE: u64 = 1;
const TAG_NOTE: u64 = 2;
const TAG_PRF_NF: u64 = 3;
const TAG_PK: u64 = 4;
const TAG_ADDR: u64 = 5;
const TAG_NFKEY: u64 = 6;
const TAG_BL_BUCKET: u64 = 7;
const TAG_IVK_SEED: u64 = 8;

// === Core hash wrapper ===

/// Domain-separated Poseidon2-Goldilocks hash.
///
/// Hashes field elements directly using the sponge construction.
/// Returns Hash32 (32 bytes) for on-chain storage.
#[inline]
fn poseidon2_gl(input: &[Goldilocks]) -> Hash32 {
    gldigest_to_hash32(&p2::poseidon2_hash(input))
}

/// Domain-separated Poseidon2-Goldilocks hash returning raw GlDigest.
#[inline]
fn _poseidon2_gl_digest(input: &[Goldilocks]) -> GlDigest {
    p2::poseidon2_hash(input)
}

// === Public hash API (matches old signatures, uses Goldilocks internally) ===

/// Domain-separated 32-byte Poseidon2 hash.
///
/// This is a compatibility wrapper. Prefer using the typed functions below
/// (mt_combine, note_commitment, etc.) for domain-separated hashing.
pub fn poseidon2_hash(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    // Pack tag + parts as bytes into Goldilocks field elements using packed encoding
    let mut all_bytes =
        Vec::with_capacity(tag.len() + parts.iter().map(|p| p.len()).sum::<usize>());
    all_bytes.extend_from_slice(tag);
    for part in parts {
        all_bytes.extend_from_slice(part);
    }
    gldigest_to_hash32(&p2::poseidon2_hash_packed_bytes(&all_bytes))
}

/// Combine two children into a parent node in the Merkle tree.
/// H(TAG_MT_NODE, level, left[0..4], right[0..4])
///
/// Input: 10 Goldilocks field elements.
#[inline]
pub fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
    let left_gl = hash32_to_gldigest(left);
    let right_gl = hash32_to_gldigest(right);
    let mut input = [Goldilocks::ZERO; 10];
    input[0] = Goldilocks::from_u64(TAG_MT_NODE);
    input[1] = Goldilocks::from_u64(level as u64);
    input[2..6].copy_from_slice(&left_gl);
    input[6..10].copy_from_slice(&right_gl);
    poseidon2_gl(&input)
}

/// Compute a note commitment.
/// H(TAG_NOTE, domain[0..4], value, rho[0..4], recipient[0..4], sender_id[0..4])
///
/// Input: 18 Goldilocks field elements.
#[inline]
pub fn note_commitment(
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> Hash32 {
    let domain_gl = hash32_to_gldigest(domain);
    let rho_gl = hash32_to_gldigest(rho);
    let recip_gl = hash32_to_gldigest(recipient);
    let sender_gl = hash32_to_gldigest(sender_id);

    let mut input = [Goldilocks::ZERO; 18];
    input[0] = Goldilocks::from_u64(TAG_NOTE);
    input[1..5].copy_from_slice(&domain_gl);
    input[5] = Goldilocks::from_u64(value);
    input[6..10].copy_from_slice(&rho_gl);
    input[10..14].copy_from_slice(&recip_gl);
    input[14..18].copy_from_slice(&sender_gl);
    poseidon2_gl(&input)
}

/// Compute a legacy note commitment (v1).
///
/// NOTE: This legacy format is kept for backward-compatible tooling and tests only.
/// Maps to the same Goldilocks construction but without sender_id.
#[inline]
pub fn note_commitment_v1(
    domain: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
) -> Hash32 {
    let domain_gl = hash32_to_gldigest(domain);
    let rho_gl = hash32_to_gldigest(rho);
    let recip_gl = hash32_to_gldigest(recipient);

    // value as u64 (Goldilocks fits u64, truncate u128)
    let value_u64 = value as u64;

    let mut input = [Goldilocks::ZERO; 14];
    input[0] = Goldilocks::from_u64(TAG_NOTE);
    input[1..5].copy_from_slice(&domain_gl);
    input[5] = Goldilocks::from_u64(value_u64);
    input[6..10].copy_from_slice(&rho_gl);
    input[10..14].copy_from_slice(&recip_gl);
    poseidon2_gl(&input)
}

/// PRF-based nullifier.
/// nf = H(TAG_PRF_NF, domain[0..4], nf_key[0..4], rho[0..4])
///
/// Input: 13 Goldilocks field elements.
#[inline]
pub fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
    let domain_gl = hash32_to_gldigest(domain);
    let nf_key_gl = hash32_to_gldigest(nf_key);
    let rho_gl = hash32_to_gldigest(rho);

    let mut input = [Goldilocks::ZERO; 13];
    input[0] = Goldilocks::from_u64(TAG_PRF_NF);
    input[1..5].copy_from_slice(&domain_gl);
    input[5..9].copy_from_slice(&nf_key_gl);
    input[9..13].copy_from_slice(&rho_gl);
    poseidon2_gl(&input)
}

// === Privacy Address Key Derivation ===

/// Derive public key from spending secret key.
/// pk = H(TAG_PK, spend_sk[0..4])
///
/// Input: 5 Goldilocks field elements.
#[inline]
pub fn pk_from_sk(spend_sk: &Hash32) -> Hash32 {
    let sk_gl = hash32_to_gldigest(spend_sk);
    let mut input = [Goldilocks::ZERO; 5];
    input[0] = Goldilocks::from_u64(TAG_PK);
    input[1..5].copy_from_slice(&sk_gl);
    poseidon2_gl(&input)
}

/// Clamp a 32-byte seed into an X25519 scalar (RFC 7748).
#[inline]
fn clamp_x25519_scalar(mut scalar: Hash32) -> [u8; 32] {
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    scalar
}

/// Derive incoming viewing key secret from domain and spending secret key.
/// ivk_sk = H(TAG_IVK_SEED, domain[0..4], spend_sk[0..4])
///
/// Input: 9 Goldilocks field elements.
#[inline]
pub fn ivk_sk_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let domain_gl = hash32_to_gldigest(domain);
    let sk_gl = hash32_to_gldigest(spend_sk);

    let mut input = [Goldilocks::ZERO; 9];
    input[0] = Goldilocks::from_u64(TAG_IVK_SEED);
    input[1..5].copy_from_slice(&domain_gl);
    input[5..9].copy_from_slice(&sk_gl);
    poseidon2_gl(&input)
}

/// Derive the incoming viewing public key (pk_ivk) from spend_sk and domain.
/// pk_ivk = X25519_BASE(clamp(ivk_sk_from_sk(domain, spend_sk)))
#[inline]
pub fn pk_ivk_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    use x25519_dalek::{PublicKey, StaticSecret};

    let ivk_sk = ivk_sk_from_sk(domain, spend_sk);
    let clamped = clamp_x25519_scalar(ivk_sk);
    let secret = StaticSecret::from(clamped);
    let public = PublicKey::from(&secret);
    *public.as_bytes()
}

/// Derive privacy recipient address from domain and public key material.
/// recipient = H(TAG_ADDR, domain[0..4], pk_spend[0..4], pk_ivk[0..4])
///
/// Input: 13 Goldilocks field elements.
#[inline]
pub fn recipient_from_pk(domain: &Hash32, pk_spend: &Hash32) -> Hash32 {
    recipient_from_pk_v2(domain, pk_spend, pk_spend)
}

/// Derive recipient using both the spend pubkey and the incoming-view pubkey.
/// recipient = H(TAG_ADDR, domain[0..4], pk_spend[0..4], pk_ivk[0..4])
#[inline]
pub fn recipient_from_pk_v2(domain: &Hash32, pk_spend: &Hash32, pk_ivk: &Hash32) -> Hash32 {
    let domain_gl = hash32_to_gldigest(domain);
    let pk_spend_gl = hash32_to_gldigest(pk_spend);
    let pk_ivk_gl = hash32_to_gldigest(pk_ivk);

    let mut input = [Goldilocks::ZERO; 13];
    input[0] = Goldilocks::from_u64(TAG_ADDR);
    input[1..5].copy_from_slice(&domain_gl);
    input[5..9].copy_from_slice(&pk_spend_gl);
    input[9..13].copy_from_slice(&pk_ivk_gl);
    poseidon2_gl(&input)
}

/// Derive privacy recipient address from domain and spending secret key.
#[inline]
pub fn recipient_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let pk = pk_from_sk(spend_sk);
    recipient_from_pk(domain, &pk)
}

/// Derive privacy recipient address from domain, spending secret key, and an explicit incoming-view pubkey.
#[inline]
pub fn recipient_from_sk_v2(domain: &Hash32, spend_sk: &Hash32, pk_ivk: &Hash32) -> Hash32 {
    let pk_spend = pk_from_sk(spend_sk);
    recipient_from_pk_v2(domain, &pk_spend, pk_ivk)
}

/// Derive nullifier key from domain and spending secret key.
/// nf_key = H(TAG_NFKEY, domain[0..4], spend_sk[0..4])
///
/// Input: 9 Goldilocks field elements.
#[inline]
pub fn nf_key_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let domain_gl = hash32_to_gldigest(domain);
    let sk_gl = hash32_to_gldigest(spend_sk);

    let mut input = [Goldilocks::ZERO; 9];
    input[0] = Goldilocks::from_u64(TAG_NFKEY);
    input[1..5].copy_from_slice(&domain_gl);
    input[5..9].copy_from_slice(&sk_gl);
    poseidon2_gl(&input)
}

/// Recompute the Merkle root from a leaf using its authentication path.
pub fn root_from_path(leaf: &Hash32, pos: u64, siblings: &[Hash32], depth: u8) -> Hash32 {
    assert_eq!(siblings.len() as u8, depth);
    let mut cur = *leaf;
    let mut idx = pos;
    for (lvl, sib) in (0..depth).zip(siblings.iter()) {
        cur = if (idx & 1) == 0 {
            mt_combine(lvl, &cur, sib)
        } else {
            mt_combine(lvl, sib, &cur)
        };
        idx >>= 1;
    }
    cur
}

/// Depth of the deny-map Merkle tree.
pub const BLACKLIST_TREE_DEPTH: u8 = 16;

/// Number of entries in each deny-map bucket leaf.
pub const BLACKLIST_BUCKET_SIZE: usize = 12;

/// Fixed-size bucket entries array stored per deny-map leaf.
pub type BlacklistBucketEntries = [Hash32; BLACKLIST_BUCKET_SIZE];

/// Return the canonical "empty bucket" entries array (all zeros).
#[inline]
pub fn empty_blacklist_bucket_entries() -> BlacklistBucketEntries {
    [[0u8; 32]; BLACKLIST_BUCKET_SIZE]
}

/// Compute the bucket leaf hash: H(TAG_BL_BUCKET, entries[0..4], ..., entries[11][0..4]).
///
/// Input: 1 (tag) + 12 * 4 (entries) = 49 Goldilocks field elements.
pub fn bl_bucket_leaf(entries: &BlacklistBucketEntries) -> Hash32 {
    let mut input = [Goldilocks::ZERO; 1 + BLACKLIST_BUCKET_SIZE * 4];
    input[0] = Goldilocks::from_u64(TAG_BL_BUCKET);
    for (i, entry) in entries.iter().enumerate() {
        let gl = hash32_to_gldigest(entry);
        let offset = 1 + i * 4;
        input[offset..offset + 4].copy_from_slice(&gl);
    }
    poseidon2_gl(&input)
}

/// Compute the leaf position (index) used by the deny-map tree from a 32-byte recipient.
pub fn blacklist_pos_from_recipient(recipient: &Hash32) -> u64 {
    let mut pos: u64 = 0;
    let depth = BLACKLIST_TREE_DEPTH as usize;
    let mut i = 0usize;
    while i < depth {
        let byte = recipient[31 - (i / 8)];
        let bit = (byte >> (i % 8)) & 1;
        pos |= (bit as u64) << i;
        i += 1;
    }
    pos
}

/// Compute the default nodes for a sparse Merkle tree of the given depth.
pub fn sparse_default_nodes(depth: u8) -> Vec<Hash32> {
    let mut out: Vec<Hash32> = Vec::with_capacity(depth as usize + 1);
    let leaf0 = bl_bucket_leaf(&empty_blacklist_bucket_entries());
    out.push(leaf0);
    for lvl in 0..depth {
        let prev = out[lvl as usize];
        out.push(mt_combine(lvl, &prev, &prev));
    }
    out
}

/// Compute the all-zero sparse Merkle root for a given depth.
pub fn sparse_default_root(depth: u8) -> Hash32 {
    let mut cur = bl_bucket_leaf(&empty_blacklist_bucket_entries());
    for lvl in 0..depth {
        cur = mt_combine(lvl, &cur, &cur);
    }
    cur
}

/// Compute the default (all-allowed) deny-map root expected by the ZK circuits.
#[inline]
pub fn default_blacklist_root() -> Hash32 {
    sparse_default_root(BLACKLIST_TREE_DEPTH)
}

/// Compute the `inv_enforce` witness used by the `note_spend` circuit.
///
/// This value is computed off-chain by the prover and passed as a private input.
/// It must match the circuit's computation exactly.
///
/// Uses Goldilocks field arithmetic: product of values * product of rho differences,
/// then modular inverse.
///
/// The enforce product matches the Nightstream RISC-V circuit's
/// `enforce_prod_digest_diff`, which multiplies ALL 4 Goldilocks elements of
/// each rho difference (not just the first limb).
pub fn inv_enforce_v2(
    in_values: &[u64],
    in_rhos: &[Hash32],
    out_values: &[u64],
    out_rhos: &[Hash32],
) -> Hash32 {
    use p3_field::Field;

    /// Convert a Hash32 to a GlDigest (4 x Goldilocks, each from 8 LE bytes).
    fn hash32_to_gl4(h: &Hash32) -> [Goldilocks; 4] {
        let mut d = [Goldilocks::ZERO; 4];
        for i in 0..4 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&h[i * 8..(i + 1) * 8]);
            d[i] = Goldilocks::from_u64(u64::from_le_bytes(buf));
        }
        d
    }

    /// Multiply acc by all 4 element-wise differences (a[i] - b[i]).
    /// Matches the circuit's `enforce_prod_digest_diff`.
    fn digest_diff_prod(mut acc: Goldilocks, a: &[Goldilocks; 4], b: &[Goldilocks; 4]) -> Goldilocks {
        for i in 0..4 {
            acc *= a[i] - b[i];
        }
        acc
    }

    let mut enforce_prod = Goldilocks::ONE;

    for &v in in_values {
        enforce_prod *= Goldilocks::from_u64(v);
    }
    for &v in out_values {
        enforce_prod *= Goldilocks::from_u64(v);
    }

    let out_gl: Vec<[Goldilocks; 4]> = out_rhos.iter().map(hash32_to_gl4).collect();
    let in_gl: Vec<[Goldilocks; 4]> = in_rhos.iter().map(hash32_to_gl4).collect();

    for out_d in &out_gl {
        for in_d in &in_gl {
            enforce_prod = digest_diff_prod(enforce_prod, out_d, in_d);
        }
    }

    if out_gl.len() == 2 {
        enforce_prod = digest_diff_prod(enforce_prod, &out_gl[0], &out_gl[1]);
    }

    let inv = enforce_prod.inverse();
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&inv.as_canonical_u64().to_le_bytes());
    out
}

// === Key wrapper types (unchanged from before) ===

/// Key for a node in the deny-map ("blacklist") Merkle tree.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct BlacklistNodeKey {
    /// Node height (0 = leaf).
    pub height: u8,
    /// Node index at this height.
    pub index: u64,
}

/// Key for a node in the note/nullifier Merkle trees.
///
/// Height 0 is a leaf. Height `depth` is the root.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct MerkleNodeKey {
    /// Node height (0 = leaf).
    pub height: u8,
    /// Node index at this height.
    pub index: u64,
}

impl fmt::Display for MerkleNodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.height, self.index)
    }
}

impl FromStr for MerkleNodeKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('_').collect();
        if parts.len() != 2 {
            return Err("Invalid format: expected height_index".to_string());
        }
        let height = parts[0]
            .parse::<u8>()
            .map_err(|e| format!("Failed to parse height: {e}"))?;
        let index = parts[1]
            .parse::<u64>()
            .map_err(|e| format!("Failed to parse index: {e}"))?;
        Ok(MerkleNodeKey { height, index })
    }
}

impl fmt::Display for BlacklistNodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.height, self.index)
    }
}

impl FromStr for BlacklistNodeKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('_').collect();
        if parts.len() != 2 {
            return Err("Invalid format: expected height_index".to_string());
        }
        let height = parts[0]
            .parse::<u8>()
            .map_err(|e| format!("Failed to parse height: {e}"))?;
        let index = parts[1]
            .parse::<u64>()
            .map_err(|e| format!("Failed to parse index: {e}"))?;
        Ok(BlacklistNodeKey { height, index })
    }
}

/// Wrapper around Hash32 that implements Display and FromStr for use in StateMap
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct NullifierKey(pub Hash32);

impl fmt::Display for NullifierKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for NullifierKey {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(NullifierKey(arr))
    }
}

/// Wrapper for Merkle roots for StateMap storage.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct RootKey(pub Hash32);

impl fmt::Display for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for RootKey {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(RootKey(arr))
    }
}

/// Composite key for pending roots: (rollup_height, idx).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct PendingRootKey {
    /// The slot height at which this root was submitted.
    pub height: u64,
    /// Index within the slot.
    pub idx: u32,
}

impl fmt::Display for PendingRootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.height, self.idx)
    }
}

impl FromStr for PendingRootKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('_').collect();
        if parts.len() != 2 {
            return Err("Invalid format: expected height_idx".to_string());
        }
        let height = parts[0]
            .parse::<u64>()
            .map_err(|e| format!("Failed to parse height: {}", e))?;
        let idx = parts[1]
            .parse::<u32>()
            .map_err(|e| format!("Failed to parse idx: {}", e))?;
        Ok(PendingRootKey { height, idx })
    }
}

/// Composite key for pending commitments: (rollup_height, commitment).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct PendingCommitmentKey {
    /// The slot height at which this commitment was submitted.
    pub height: u64,
    /// The commitment hash.
    pub commitment: Hash32,
}

impl fmt::Display for PendingCommitmentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.height, hex::encode(self.commitment))
    }
}

impl FromStr for PendingCommitmentKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(2, '_').collect();
        if parts.len() != 2 {
            return Err("Invalid format: expected height_commitment".to_string());
        }
        let height = parts[0]
            .parse::<u64>()
            .map_err(|e| format!("Failed to parse height: {}", e))?;
        let commitment_bytes =
            hex::decode(parts[1]).map_err(|e| format!("Failed to parse commitment: {}", e))?;
        if commitment_bytes.len() != 32 {
            return Err("Commitment must be 32 bytes".to_string());
        }
        let mut commitment = [0u8; 32];
        commitment.copy_from_slice(&commitment_bytes);
        Ok(PendingCommitmentKey { height, commitment })
    }
}

/// Composite key for pending nullifiers: (rollup_height, nullifier).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct PendingNullifierKey {
    /// The slot height at which this nullifier was submitted.
    pub height: u64,
    /// The nullifier hash.
    pub nullifier: Hash32,
}

impl fmt::Display for PendingNullifierKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.height, hex::encode(self.nullifier))
    }
}

impl FromStr for PendingNullifierKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(2, '_').collect();
        if parts.len() != 2 {
            return Err("Invalid format: expected height_nullifier".to_string());
        }
        let height = parts[0]
            .parse::<u64>()
            .map_err(|e| format!("Failed to parse height: {}", e))?;
        let nullifier_bytes =
            hex::decode(parts[1]).map_err(|e| format!("Failed to parse nullifier: {}", e))?;
        if nullifier_bytes.len() != 32 {
            return Err("Nullifier must be 32 bytes".to_string());
        }
        let mut nullifier = [0u8; 32];
        nullifier.copy_from_slice(&nullifier_bytes);
        Ok(PendingNullifierKey { height, nullifier })
    }
}

/// Prefix type for iterating pending commitments by height.
#[derive(Debug, Clone, Copy, BorshSerialize)]
pub struct PendingCommitmentPrefix {
    /// The slot height to query pending commitments for.
    pub height: u64,
}

/// Prefix type for iterating pending nullifiers by height.
#[derive(Debug, Clone, Copy, BorshSerialize)]
pub struct PendingNullifierPrefix {
    /// The slot height to query pending nullifiers for.
    pub height: u64,
}

/// Compute the default nodes for a dense Merkle tree used by commitments/nullifiers.
///
/// Returns a vector of length `depth + 1` where:
/// - `out[0]` is the default leaf (`0x00..00`)
/// - `out[h]` is the default node at height `h`
pub fn mt_default_nodes(depth: u8) -> Vec<Hash32> {
    let mut out: Vec<Hash32> = Vec::with_capacity(depth as usize + 1);
    out.push([0u8; 32]);
    for lvl in 0..depth {
        let prev = out[lvl as usize];
        out.push(mt_combine(lvl, &prev, &prev));
    }
    out
}

/// Compute the all-default root for the note/nullifier Merkle tree.
#[inline]
pub fn mt_default_root(depth: u8) -> Hash32 {
    mt_default_nodes(depth)[depth as usize]
}
// (tests live in `tests/ivk_crypto_tests.rs`)
