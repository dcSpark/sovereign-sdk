//! Domain-separated Poseidon2 hash functions for privacy-preserving operations.
//!
//! This module uses Ligetron's Poseidon2, which is compatible with the ZK circuit.
//! Using the same Poseidon2 implementation ensures hash consistency between
//! native Rust code and the Ligero circuit.
//!
//! Domain separation is achieved by prepending unique domain tags to each input type,
//! preventing cross-domain collisions and attacks.

use std::fmt;
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use ligetron::poseidon2_hash_bytes as ligetron_hash_bytes;
use serde::{Deserialize, Serialize};

/// 32-byte hash output.
pub type Hash32 = [u8; 32];

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

/// Wrapper for Merkle roots so we can store membership in StateMap (NOMT-backed).
/// This enables permanent indexing of all historical roots for long-range anchor validation.
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
/// This allows O(1) append operations per root, avoiding VecDeque serialization overhead.
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
    /// Rollup height this root was created in
    pub height: u64,
    /// Sequential index within the block
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
/// Uses the commitment hash itself as the unique identifier to avoid conflicts
/// during parallel execution. Each commitment is unique, so each key is unique.
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
    /// Rollup height this commitment was created in
    pub height: u64,
    /// The commitment hash (unique identifier)
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
/// Uses the nullifier hash itself as the unique identifier to avoid conflicts
/// during parallel execution.
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
    /// Rollup height this nullifier was spent in
    pub height: u64,
    /// The nullifier hash (unique identifier)
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
/// When Borsh-serialized, this produces the prefix bytes of PendingCommitmentKey.
/// Used with StateMap::iter_prefix to enumerate all commitments for a given height.
#[derive(Debug, Clone, Copy, BorshSerialize)]
pub struct PendingCommitmentPrefix {
    /// Rollup height to iterate
    pub height: u64,
}

/// Prefix type for iterating pending nullifiers by height.
/// When Borsh-serialized, this produces the prefix bytes of PendingNullifierKey.
/// Used with StateMap::iter_prefix to enumerate all nullifiers for a given height.
#[derive(Debug, Clone, Copy, BorshSerialize)]
pub struct PendingNullifierPrefix {
    /// Rollup height to iterate
    pub height: u64,
}

/// Domain-separated 32-byte Poseidon2 hash using Ligetron's implementation.
/// `tag` must be unique per domain (e.g., "MT_NODE_V1", "NOTE_V2", "PRF_NF_V1").
/// This provides collision resistance between different hash use cases.
///
/// Uses Ligetron's native Poseidon2 to ensure consistency with the ZK circuit.
pub fn poseidon2_hash(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    // Concatenate tag and all parts
    let mut input = Vec::with_capacity(tag.len() + parts.iter().map(|p| p.len()).sum::<usize>());
    input.extend_from_slice(tag);
    for part in parts {
        input.extend_from_slice(part);
    }

    // Use Ligetron's native Poseidon2 (consistent with the circuit)
    ligetron_hash_bytes(&input).to_bytes_be()
}

/// Domain tags as fixed-size arrays (avoids const evaluation issues)
const MT_TAG: &[u8; 10] = b"MT_NODE_V1";
const NOTE_TAG: &[u8; 7] = b"NOTE_V2";
const NOTE_V1_TAG: &[u8; 7] = b"NOTE_V1";
const NF_TAG: &[u8; 9] = b"PRF_NF_V1";

/// Combine two children into a parent node in the Merkle tree.
/// Uses domain tag "MT_NODE_V1" with level to prevent cross-level collisions.
/// Optimized to avoid heap allocations by using a fixed-size buffer.
#[inline]
pub fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
    // Fixed-size buffer: tag (10 bytes) + level (1 byte) + left (32 bytes) + right (32 bytes) = 75 bytes
    let mut buf = [0u8; 10 + 1 + 32 + 32];
    buf[..10].copy_from_slice(MT_TAG);
    buf[10] = level;
    buf[11..43].copy_from_slice(left);
    buf[43..].copy_from_slice(right);

    ligetron_hash_bytes(&buf).to_bytes_be()
}

/// Compute a note commitment.
/// Commits to: domain tag, value, rho, recipient binding, and `sender_id`.
/// Uses domain tag "NOTE_V2" for domain separation.
/// Optimized to avoid heap allocations by using a fixed-size buffer.
#[inline]
pub fn note_commitment(
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> Hash32 {
    // Fixed-size buffer:
    // tag (7) + domain (32) + value_le_16 (16) + rho (32) + recipient (32) + sender_id (32) = 151 bytes
    let mut buf = [0u8; 7 + 32 + 16 + 32 + 32 + 32];
    buf[..7].copy_from_slice(NOTE_TAG);
    buf[7..39].copy_from_slice(domain);
    // Encode value as 16-byte LE, zero-extended from u64.
    buf[39..47].copy_from_slice(&value.to_le_bytes());
    // buf[47..55] are already zero-initialized.
    buf[55..87].copy_from_slice(rho);
    buf[87..119].copy_from_slice(recipient);
    buf[119..151].copy_from_slice(sender_id);

    ligetron_hash_bytes(&buf).to_bytes_be()
}

/// Compute a legacy note commitment (v1).
///
/// NOTE: This legacy format is kept for backward-compatible tooling and tests only.
/// The current `note_spend_guest` circuit uses `NOTE_V2`.
#[inline]
pub fn note_commitment_v1(
    domain: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
) -> Hash32 {
    let v = value.to_le_bytes();
    // tag (7) + domain (32) + value_le_16 (16) + rho (32) + recipient (32) = 119 bytes
    let mut buf = [0u8; 7 + 32 + 16 + 32 + 32];
    buf[..7].copy_from_slice(NOTE_V1_TAG);
    buf[7..39].copy_from_slice(domain);
    buf[39..55].copy_from_slice(&v);
    buf[55..87].copy_from_slice(rho);
    buf[87..].copy_from_slice(recipient);
    ligetron_hash_bytes(&buf).to_bytes_be()
}

/// PRF-based nullifier (position removed, follows Zcash/ZK standard pattern).
/// nf = Poseidon2("PRF_NF_V1" || domain || nf_key || rho)
///
/// - `nf_key` is derived from the spender's secret
/// - `rho` is the note's randomness (part of the note opening)
///
/// This makes nullifiers position-agnostic: spending the same note across different
/// anchors yields the same `nf`, enabling reliable double-spend detection across forks.
/// Optimized to avoid heap allocations by using a fixed-size buffer.
#[inline]
pub fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
    // Fixed-size buffer: tag (9 bytes) + domain (32 bytes) + nf_key (32 bytes) + rho (32 bytes) = 105 bytes
    let mut buf = [0u8; 9 + 32 + 32 + 32];
    buf[..9].copy_from_slice(NF_TAG);
    buf[9..41].copy_from_slice(domain);
    buf[41..73].copy_from_slice(nf_key);
    buf[73..].copy_from_slice(rho);

    ligetron_hash_bytes(&buf).to_bytes_be()
}

// === Privacy Address Key Derivation ===
// These functions derive public key (pk) and recipient from spend_sk.
// The circuit uses these to bind spending authorization to note ownership.

const PK_TAG: &[u8; 5] = b"PK_V1";
const ADDR_TAG: &[u8; 7] = b"ADDR_V2";
const NFKEY_TAG: &[u8; 8] = b"NFKEY_V1";
const IVK_SEED_TAG: &[u8; 11] = b"IVK_SEED_V1";

/// Derive public key from spending secret key.
/// pk = H("PK_V1" || spend_sk)
#[inline]
pub fn pk_from_sk(spend_sk: &Hash32) -> Hash32 {
    let mut buf = [0u8; 5 + 32];
    buf[..5].copy_from_slice(PK_TAG);
    buf[5..].copy_from_slice(spend_sk);
    ligetron_hash_bytes(&buf).to_bytes_be()
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
/// ivk_sk = H("IVK_SEED_V1" || domain || spend_sk)
///
/// The receiver uses this to decrypt notes sent to them.
#[inline]
pub fn ivk_sk_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let mut buf = [0u8; 11 + 32 + 32];
    buf[..11].copy_from_slice(IVK_SEED_TAG);
    buf[11..43].copy_from_slice(domain);
    buf[43..].copy_from_slice(spend_sk);
    ligetron_hash_bytes(&buf).to_bytes_be()
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
/// recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)
///
/// This is the internal 32-byte "recipient" value used in note commitments.
/// For the user-facing bech32 address, use PrivacyAddress::from_pk(pk).
#[inline]
pub fn recipient_from_pk(domain: &Hash32, pk_spend: &Hash32) -> Hash32 {
    // Backward-compatible default: if callers only have one key, treat `pk_ivk == pk_spend`.
    recipient_from_pk_v2(domain, pk_spend, pk_spend)
}

/// Derive recipient using both the spend pubkey and the incoming-view pubkey.
#[inline]
pub fn recipient_from_pk_v2(domain: &Hash32, pk_spend: &Hash32, pk_ivk: &Hash32) -> Hash32 {
    let mut buf = [0u8; 7 + 32 + 32 + 32];
    buf[..7].copy_from_slice(ADDR_TAG);
    buf[7..39].copy_from_slice(domain);
    buf[39..71].copy_from_slice(pk_spend);
    buf[71..103].copy_from_slice(pk_ivk);
    ligetron_hash_bytes(&buf).to_bytes_be()
}

/// Derive privacy recipient address from domain and spending secret key.
/// This is a convenience function: recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk),
/// with the backward-compatible default `pk_ivk == pk_spend`.
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
/// nf_key = H("NFKEY_V1" || domain || spend_sk)
#[inline]
pub fn nf_key_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let mut buf = [0u8; 8 + 32 + 32];
    buf[..8].copy_from_slice(NFKEY_TAG);
    buf[8..40].copy_from_slice(domain);
    buf[40..].copy_from_slice(spend_sk);
    ligetron_hash_bytes(&buf).to_bytes_be()
}

/// Recompute the Merkle root from a leaf using its authentication path.
/// Verifies that a leaf with given siblings can produce the claimed root.
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

// (tests live in `tests/ivk_crypto_tests.rs`)
