//! Incremental/cached growable Merkle tree with O(log N) updates.
//! Stores all internal nodes and updates only ancestors on leaf changes.
//! This provides O(1) root access and O(log N) authentication paths without rehashing.
//!
//! The tree supports dynamic capacity growth using the Verdict-style doubling technique:
//! when full, it embeds the current tree as the left subtree of a deeper tree,
//! with the right subtree initialized to all-zero defaults.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::hash::{mt_combine, Hash32};

/// Maximum tree depth supported by the guest circuit.
/// The guest uses `1u64 << depth` for position bounds, so depth must be ≤ 63
/// to avoid undefined behavior from shifting by 64 bits.
pub const MAX_TREE_DEPTH: u8 = 63;

/// An incremental Merkle tree that caches all internal nodes.
/// - levels[0] = leaves
/// - levels[depth] = root (single node)
/// Memory cost: ~2N nodes for a tree with N leaves (~4-5 MiB at depth 16)
/// Time complexity:
///   - new(): O(N) memory allocation, O(depth) hashing for default nodes
///   - set_leaf(): O(depth) - only updates ancestors
///   - root(): O(1) - direct access
///   - open(): O(depth) - no hashing, just sibling lookups
///   - grow_to_fit(): O(N) memory reallocation when doubling capacity
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize, PartialEq, Eq)]
pub struct MerkleTree {
    depth: u8,
    /// levels[i] contains 2^(depth - i) nodes
    /// levels[0] = leaves, levels[depth] = root (single node)
    levels: Vec<Vec<Hash32>>,
    /// Cached default nodes: defaults[i] = hash of all-zero subtree at level i.
    /// This avoids recomputing defaults on each grow operation.
    /// Length is always depth + 1 (indices 0..=depth).
    defaults: Vec<Hash32>,
}

impl MerkleTree {
    /// Create a tree of `2^depth` leaves initialized to zero.
    /// Precomputes default "all-zero subtree" hashes once, then fills levels with these defaults.
    ///
    /// # Panics
    /// Panics if `depth > MAX_TREE_DEPTH` (63).
    pub fn new(depth: u8) -> Self {
        assert!(
            depth <= MAX_TREE_DEPTH,
            "MerkleTree::new: depth {} exceeds MAX_TREE_DEPTH {}",
            depth,
            MAX_TREE_DEPTH
        );

        let n = 1usize << depth;

        // Precompute default node for each level (O(depth) hashes)
        // defaults[i] = hash of all-zero subtree at level i
        let mut defaults = Vec::with_capacity(depth as usize + 1);
        defaults.push([0u8; 32]); // level 0: zero leaf
        for lvl in 0..depth {
            let next = mt_combine(lvl, &defaults[lvl as usize], &defaults[lvl as usize]);
            defaults.push(next);
        }

        // Allocate and initialize each level with its default node (no hashing here)
        let mut levels = Vec::with_capacity(depth as usize + 1);
        levels.push(vec![defaults[0]; n]); // leaves at level 0
        for lvl in 1..=depth as usize {
            let len = n >> lvl; // 2^(depth - lvl)
            levels.push(vec![defaults[lvl]; len]);
        }

        Self {
            depth,
            levels,
            defaults,
        }
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

    /// Ensure the tree can hold at least `min_leaves` leaves.
    /// If not, repeatedly double the capacity by embedding the old tree
    /// as the left subtree of a deeper tree and filling the right subtree
    /// with default (all-zero) nodes.
    ///
    /// Uses the Verdict-style doubling technique: given the root `r` of a `2^i`-sized tree,
    /// the root of a `2^{i+1}`-sized tree is `hash(r, r₀)`, where `r₀` is the root of
    /// the all-zero default tree of size `2^i`.
    ///
    /// # Panics
    /// Panics if the required depth would exceed `MAX_TREE_DEPTH` (63).
    pub fn grow_to_fit(&mut self, min_leaves: usize) {
        if min_leaves <= self.len() {
            return;
        }
        while self.len() < min_leaves {
            self.grow_one();
        }
    }

    /// Double the capacity of the tree once.
    ///
    /// New tree semantics:
    /// - depth' = depth + 1
    /// - leaves[0..old_len) = old leaves
    /// - leaves[old_len..2*old_len) = zero leaves
    /// - left subtree = old tree
    /// - right subtree = default tree of depth `old_depth`
    ///
    /// New root = mt_combine(old_depth, old_root, default_root(old_depth)).
    ///
    /// # Panics
    /// Panics if the new depth would exceed `MAX_TREE_DEPTH`.
    fn grow_one(&mut self) {
        let old_depth = self.depth;
        let new_depth = old_depth.checked_add(1).expect("MerkleTree depth overflow");

        assert!(
            new_depth <= MAX_TREE_DEPTH,
            "MerkleTree cannot grow beyond depth {} (requested {})",
            MAX_TREE_DEPTH,
            new_depth
        );

        let old_levels = std::mem::take(&mut self.levels);
        let old_leaf_len = 1usize << (old_depth as usize);
        let new_leaf_len = 1usize << (new_depth as usize);

        // Extend defaults by one level (O(1) hash instead of O(depth) hashes)
        // The new default at level `new_depth` = hash(default[old_depth], default[old_depth])
        let new_default = mt_combine(
            old_depth,
            &self.defaults[old_depth as usize],
            &self.defaults[old_depth as usize],
        );
        self.defaults.push(new_default);

        let mut levels: Vec<Vec<Hash32>> = Vec::with_capacity(new_depth as usize + 1);

        // Leaves: old tree in left half, right half default
        let mut leaves = vec![self.defaults[0]; new_leaf_len];
        leaves[..old_leaf_len].copy_from_slice(&old_levels[0]);
        levels.push(leaves);

        // Internal levels 1..=old_depth: copy left subtree, right subtree stays default
        for lvl in 1..=old_depth as usize {
            let len_new = new_leaf_len >> lvl; // 2^(new_depth - lvl)
            let len_old = old_levels[lvl].len();
            let mut nodes = vec![self.defaults[lvl]; len_new];
            nodes[..len_old].copy_from_slice(&old_levels[lvl]);
            levels.push(nodes);
        }

        // New root level: combine old root with default right-subtree root
        let left_root = old_levels[old_depth as usize][0];
        let right_root = self.defaults[old_depth as usize]; // default root for depth `old_depth`
        let root = mt_combine(old_depth, &left_root, &right_root);
        levels.push(vec![root]);

        self.depth = new_depth;
        self.levels = levels;
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
