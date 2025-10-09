//! Simple fixed-depth Merkle tree with append/overwrite by index.
//! Uses the `toy_hash` combiner for internal nodes.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::hash::{mt_combine, Hash32};

/// A compact Merkle tree storing only the leaves; internal nodes are recomputed on demand.
/// This is fine for tests and small sizes; swap for an incremental tree if needed.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize, PartialEq, Eq)]
pub struct MerkleTree {
    depth: u8,
    leaves: Vec<Hash32>,
}

impl MerkleTree {
    /// Create a tree of `2^depth` leaves initialized to zero.
    pub fn new(depth: u8) -> Self {
        let n = 1usize << depth;
        Self {
            depth,
            leaves: vec![[0u8; 32]; n],
        }
    }

    /// Return the tree depth (number of levels from leaves to root).
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Set the leaf at `index` to `val`.
    pub fn set_leaf(&mut self, index: usize, val: Hash32) {
        assert!(index < self.leaves.len());
        self.leaves[index] = val;
    }

    /// Fetch the leaf at `index`.
    pub fn leaf(&self, index: usize) -> Hash32 {
        self.leaves[index]
    }

    /// Compute the current Merkle root by folding all levels.
    pub fn root(&self) -> Hash32 {
        let mut level = self.leaves.clone();
        let mut depth = 0u8;
        while level.len() > 1 {
            let mut next = Vec::with_capacity((level.len() + 1) / 2);
            for pair in level.chunks(2) {
                let left = pair[0];
                let right = if pair.len() == 2 { pair[1] } else { [0u8; 32] };
                next.push(mt_combine(depth, &left, &right));
            }
            level = next;
            depth += 1;
        }
        level[0]
    }

    /// Return the Merkle authentication path (siblings from leaf to root).
    pub fn open(&self, index: usize) -> Vec<Hash32> {
        assert!(index < self.leaves.len());
        let mut idx = index;
        let mut level = self.leaves.clone();
        let mut depth = 0u8;
        let mut path = Vec::with_capacity(self.depth as usize);

        while level.len() > 1 {
            // collect sibling for current idx
            let sib_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
            let sib = if sib_idx < level.len() { level[sib_idx] } else { [0u8; 32] };
            path.push(sib);

            // fold to next level
            let mut next = Vec::with_capacity((level.len() + 1) / 2);
            for pair in level.chunks(2) {
                let left = pair[0];
                let right = if pair.len() == 2 { pair[1] } else { [0u8; 32] };
                next.push(mt_combine(depth, &left, &right));
            }
            idx /= 2;
            level = next;
            depth += 1;
        }

        assert_eq!(path.len() as u8, self.depth);
        path
    }

    /// Get the number of leaves in the tree
    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    /// Check if the tree is empty
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }
}

