//! Domain-separated Poseidon2 hash functions for privacy-preserving operations.
//!
//! This module uses Poseidon2, a ZK-friendly hash function optimized for zero-knowledge
//! proof systems. Poseidon2 is significantly faster than the original Poseidon and is
//! designed to work efficiently with arithmetic circuits.
//!
//! Domain separation is achieved by prepending unique domain tags to each input type,
//! preventing cross-domain collisions and attacks.

use std::cell::RefCell;
use std::fmt;
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use qp_poseidon_core::Poseidon2Core;
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

// Thread-local Poseidon2 hasher instance (deterministic with fixed seed).
// Using thread-local instances avoids repeated allocations and initialization overhead
// while ensuring thread-safety without synchronization overhead.
thread_local! {
    static POSEIDON: RefCell<Poseidon2Core> = RefCell::new(Poseidon2Core::new());
}

/// Domain-separated 32-byte Poseidon2 hash.
/// `tag` must be unique per domain (e.g., "MT_NODE_V1", "NOTE_V1", "NF_V1").
/// This provides collision resistance between different hash use cases.
pub fn poseidon2_hash(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    // Concatenate tag and all parts
    let mut input = Vec::with_capacity(tag.len() + parts.iter().map(|p| p.len()).sum::<usize>());
    input.extend_from_slice(tag);
    for part in parts {
        input.extend_from_slice(part);
    }

    POSEIDON.with(|h| h.borrow().hash_padded(&input))
}

/// Domain tags as fixed-size arrays (avoids const evaluation issues)
const MT_TAG: &[u8; 10] = b"MT_NODE_V1";
const NOTE_TAG: &[u8; 7] = b"NOTE_V1";
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

    POSEIDON.with(|h| h.borrow().hash_padded(&buf))
}

/// Compute a note commitment.
/// Commits to: domain tag, value, randomness, and recipient binding.
/// Uses domain tag "NOTE_V1" for domain separation.
/// Optimized to avoid heap allocations by using a fixed-size buffer.
#[inline]
pub fn note_commitment(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32) -> Hash32 {
    let v = value.to_le_bytes();
    // Fixed-size buffer: tag (7 bytes) + domain (32 bytes) + value (16 bytes) + rho (32 bytes) + recipient (32 bytes) = 119 bytes
    let mut buf = [0u8; 7 + 32 + 16 + 32 + 32];
    buf[..7].copy_from_slice(NOTE_TAG);
    buf[7..39].copy_from_slice(domain);
    buf[39..55].copy_from_slice(&v);
    buf[55..87].copy_from_slice(rho);
    buf[87..].copy_from_slice(recipient);

    POSEIDON.with(|h| h.borrow().hash_padded(&buf))
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

    POSEIDON.with(|h| h.borrow().hash_padded(&buf))
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
