//! Pending output log for deferred Merkle appends

use borsh::{BorshDeserialize, BorshSerialize};
use crate::hash::Hash32;

/// Optional wrapper, kept for clarity if you want to type-tag commitments elsewhere.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Commitment(pub Hash32);

/// A single output commitment queued for end-of-block positioning
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
pub struct PendingOutput {
    /// The commitment to be inserted into the tree
    pub cm: Hash32,
}

/// Append-only log of outputs for the current block. Drained in epilogue.
#[derive(BorshSerialize, BorshDeserialize, Default, Clone, Debug)]
pub struct PendingLog {
    /// Append-only within a block
    pub outputs: Vec<PendingOutput>,
}

