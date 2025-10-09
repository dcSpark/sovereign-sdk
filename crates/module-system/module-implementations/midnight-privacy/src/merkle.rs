//! Incremental/cached fixed-depth Merkle tree with O(log N) updates.
//! Stores all internal nodes and updates only ancestors on leaf changes.
//! This provides O(1) root access and O(log N) authentication paths without rehashing.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::hash::{mt_combine, Hash32};

/// An incremental Merkle tree that caches all internal nodes.
/// - levels[0] = leaves
/// - levels[depth] = root (single node)
/// Memory cost: ~2N nodes for a tree with N leaves (~4-5 MiB at depth 16)
/// Time complexity:
///   - new(): O(N) memory allocation, O(depth) hashing for default nodes
///   - set_leaf(): O(depth) - only updates ancestors
///   - root(): O(1) - direct access
///   - open(): O(depth) - no hashing, just sibling lookups
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize, PartialEq, Eq)]
pub struct MerkleTree {
    depth: u8,
    /// levels[i] contains 2^(depth - i) nodes
    /// levels[0] = leaves, levels[depth] = root (single node)
    levels: Vec<Vec<Hash32>>,
}

impl MerkleTree {
    /// Create a tree of `2^depth` leaves initialized to zero.
    /// Precomputes default "all-zero subtree" hashes once, then fills levels with these defaults.
    pub fn new(depth: u8) -> Self {
        let n = 1usize << depth;

        // Precompute default node for each level (O(depth) hashes)
        // default[i] = hash of all-zero subtree at level i
        let mut default = Vec::with_capacity(depth as usize + 1);
        default.push([0u8; 32]); // level 0: zero leaf
        for lvl in 0..depth {
            let next = mt_combine(lvl, &default[lvl as usize], &default[lvl as usize]);
            default.push(next);
        }

        // Allocate and initialize each level with its default node (no hashing here)
        let mut levels = Vec::with_capacity(depth as usize + 1);
        levels.push(vec![default[0]; n]); // leaves at level 0
        for lvl in 1..=depth as usize {
            let len = n >> lvl; // 2^(depth - lvl)
            levels.push(vec![default[lvl]; len]);
        }

        Self { depth, levels }
    }

    /// Return the tree depth (number of levels from leaves to root).
    #[inline]
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Get the number of leaves in the tree.
    #[inline]
    pub fn len(&self) -> usize {
        self.levels[0].len()
    }

    /// Check if the tree is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Set the leaf at `index` to `val` and update all ancestors in O(log N).
    /// This performs exactly `depth` hashes to update the path from leaf to root.
    pub fn set_leaf(&mut self, index: usize, val: Hash32) {
        assert!(index < self.len(), "index {} out of bounds", index);
        
        // Update the leaf
        self.levels[0][index] = val;

        // Update all ancestors up to the root
        let mut idx = index;
        for lvl in 0..self.depth as usize {
            let parent = idx / 2;
            
            // Get both children at current level
            let left = self.levels[lvl][parent * 2];
            let right = if parent * 2 + 1 < self.levels[lvl].len() {
                self.levels[lvl][parent * 2 + 1]
            } else {
                [0u8; 32] // right child doesn't exist (shouldn't happen for power-of-2 trees)
            };
            
            // Update parent at next level
            self.levels[lvl + 1][parent] = mt_combine(lvl as u8, &left, &right);
            idx = parent;
        }
    }

    /// Fetch the leaf at `index`.
    #[inline]
    pub fn leaf(&self, index: usize) -> Hash32 {
        self.levels[0][index]
    }

    /// Get the current Merkle root in O(1) - no hashing required.
    #[inline]
    pub fn root(&self) -> Hash32 {
        self.levels[self.depth as usize][0]
    }

    /// Return the Merkle authentication path (siblings from leaf to root) in O(depth).
    /// No hashing is performed - siblings are read directly from cached levels.
    pub fn open(&self, index: usize) -> Vec<Hash32> {
        assert!(index < self.len(), "index {} out of bounds", index);
        
        let mut idx = index;
        let mut path = Vec::with_capacity(self.depth as usize);
        
        for lvl in 0..self.depth as usize {
            // Get sibling at current level
            let sib_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
            let sib = if sib_idx < self.levels[lvl].len() {
                self.levels[lvl][sib_idx]
            } else {
                [0u8; 32] // implicit zero sibling if tree width is odd
            };
            path.push(sib);
            idx /= 2;
        }
        
        assert_eq!(path.len(), self.depth as usize);
        path
    }
}

