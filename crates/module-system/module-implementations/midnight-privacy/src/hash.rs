//! Domain-separated Poseidon2 hash functions for privacy-preserving operations.
//!
//! This module uses Poseidon2, a ZK-friendly hash function optimized for zero-knowledge
//! proof systems. Poseidon2 is significantly faster than the original Poseidon and is
//! designed to work efficiently with arithmetic circuits.
//!
//! Domain separation is achieved by prepending unique domain tags to each input type,
//! preventing cross-domain collisions and attacks.

use std::fmt;
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use qp_poseidon_core::Poseidon2Core;
use serde::{Deserialize, Serialize};

/// 32-byte hash output.
pub type Hash32 = [u8; 32];

/// Wrapper around Hash32 that implements Display and FromStr for use in StateMap
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
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

/// Global Poseidon2 hasher instance (deterministic with fixed seed)
fn get_hasher() -> Poseidon2Core {
    Poseidon2Core::new()
}

/// Domain-separated 32-byte Poseidon2 hash.
/// `tag` must be unique per domain (e.g., "MT_NODE_V1", "NOTE_V1", "NF_V1").
/// This provides collision resistance between different hash use cases.
pub fn poseidon2_hash(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    let hasher = get_hasher();
    
    // Concatenate tag and all parts
    let mut input = Vec::with_capacity(tag.len() + parts.iter().map(|p| p.len()).sum::<usize>());
    input.extend_from_slice(tag);
    for part in parts {
        input.extend_from_slice(part);
    }
    
    hasher.hash_padded(&input)
}

/// Combine two children into a parent node in the Merkle tree.
/// Uses domain tag "MT_NODE_V1" with level to prevent cross-level collisions.
pub fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
    let lvl = [level];
    poseidon2_hash(b"MT_NODE_V1", &[&lvl, left, right])
}

/// Compute a note commitment.
/// Commits to: domain tag, value, randomness, and recipient binding.
/// Uses domain tag "NOTE_V1" for domain separation.
pub fn note_commitment(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32) -> Hash32 {
    let v = value.to_le_bytes();
    poseidon2_hash(b"NOTE_V1", &[domain, &v, rho, recipient])
}

/// PRF-based nullifier (position removed, follows Zcash/ZK standard pattern).
/// nf = Poseidon2("PRF_NF_V1" || domain || nf_key || rho)
///
/// - `nf_key` is derived from the spender's secret
/// - `rho` is the note's randomness (part of the note opening)
///
/// This makes nullifiers position-agnostic: spending the same note across different
/// anchors yields the same `nf`, enabling reliable double-spend detection across forks.
pub fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
    poseidon2_hash(b"PRF_NF_V1", &[domain, nf_key, rho])
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
