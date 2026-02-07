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

    /// Create a tree from a prefix of filled leaves, rebuilding all internal nodes bottom-up.
    ///
    /// Semantics:
    /// - The tree has exactly `2^depth` leaves.
    /// - Leaves in `0..filled_leaves.len()` are set to `filled_leaves`.
    /// - Remaining leaves are zero (the default leaf value).
    ///
    /// This is substantially faster than calling `set_leaf()` in a loop because it hashes
    /// only the internal nodes that depend on the filled prefix (roughly O(filled_leaves.len())
    /// hashes for sparse prefixes, and O(2^depth) hashes in the worst case).
    ///
    /// # Panics
    /// Panics if `filled_leaves.len() > 2^depth`.
    pub fn from_filled_leaves(depth: u8, filled_leaves: &[Hash32]) -> Self {
        // NOTE: `filled_leaves` is a *prefix*; all leaves beyond it are defined to be zero.
        let capacity = 1usize << (depth as usize);
        assert!(
            filled_leaves.len() <= capacity,
            "MerkleTree::from_filled_leaves: filled_leaves {} exceeds capacity {} for depth {}",
            filled_leaves.len(),
            capacity,
            depth
        );

        if filled_leaves.is_empty() {
            return MerkleTree::new(depth);
        }

        let mut tree = MerkleTree::new(depth);
        tree.levels[0][..filled_leaves.len()].copy_from_slice(filled_leaves);

        // Optimization: only recompute nodes whose covered leaf-range intersects the filled prefix.
        //
        // For a prefix [0, filled), the number of nodes at level `lvl` (0=leaves) that can differ
        // from the all-default value is ceil(filled / 2^lvl), i.e. the first N nodes.
        // We reuse the fact that `MerkleTree::new()` pre-filled all nodes with correct defaults.
        let mut affected = filled_leaves.len(); // potentially-non-default nodes at this level
        for lvl in 0..depth as usize {
            // Parents at next level that cover any part of the prefix.
            affected = (affected + 1) >> 1; // ceil(prev / 2)
            debug_assert!(affected >= 1);
            debug_assert!(affected <= tree.levels[lvl + 1].len());

            for parent in 0..affected {
                let left = tree.levels[lvl][parent * 2];
                let right = tree.levels[lvl][parent * 2 + 1];
                tree.levels[lvl + 1][parent] = mt_combine(lvl as u8, &left, &right);
            }
        }

        tree
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
            let parent = idx >> 1;
            // For this tree implementation, all levels are power-of-two width, so both children exist.
            let left = self.levels[lvl][parent * 2];
            let right = self.levels[lvl][parent * 2 + 1];

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
            // All levels are power-of-two width; sibling index is always in-bounds.
            path.push(self.levels[lvl][idx ^ 1]);
            idx >>= 1;
        }

        assert_eq!(path.len(), self.depth as usize);
        path
    }

    /// Return a reference to the leaf-level data.
    #[inline]
    pub fn leaves(&self) -> &[Hash32] {
        &self.levels[0]
    }

    /// Efficiently set a contiguous range of leaves starting at `start` and rebuild
    /// only the affected internal nodes bottom-up.
    ///
    /// This is much faster than calling `set_leaf()` in a loop for contiguous updates:
    /// total hashing work is O(values.len()) instead of O(values.len() * depth).
    ///
    /// # Panics
    /// Panics if `start + values.len() > self.len()`.
    pub fn set_leaves_contiguous(&mut self, start: usize, values: &[Hash32]) {
        if values.is_empty() {
            return;
        }
        let end = start + values.len();
        assert!(
            end <= self.len(),
            "set_leaves_contiguous: range {}..{} exceeds tree len {}",
            start,
            end,
            self.len()
        );

        // Set leaf values via memcpy.
        self.levels[0][start..end].copy_from_slice(values);

        // Rebuild only the affected internal nodes bottom-up.
        // At each level, track which parent nodes cover the modified range.
        let mut range_start = start;
        let mut range_end = end;
        for lvl in 0..self.depth as usize {
            let parent_start = range_start / 2;
            let parent_end = (range_end + 1) / 2;

            for parent in parent_start..parent_end {
                let left = self.levels[lvl][parent * 2];
                let right = self.levels[lvl][parent * 2 + 1];
                self.levels[lvl + 1][parent] = mt_combine(lvl as u8, &left, &right);
            }

            range_start = parent_start;
            range_end = parent_end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MerkleTree;
    use crate::hash::Hash32;

    #[test]
    fn from_filled_leaves_empty_matches_new() {
        let depth: u8 = 8;
        let via_new = MerkleTree::new(depth);
        let via_bulk = MerkleTree::from_filled_leaves(depth, &[]);
        assert_eq!(via_new.root(), via_bulk.root());
        for idx in [0usize, 1, 2, (1usize << depth) - 1] {
            assert_eq!(via_new.open(idx), via_bulk.open(idx));
        }
    }

    #[test]
    fn from_filled_leaves_matches_set_leaf() {
        let depth: u8 = 8;
        let capacity = 1usize << (depth as usize);
        let filled = 200usize;
        assert!(filled <= capacity);

        let mut leaves: Vec<Hash32> = Vec::with_capacity(filled);
        for i in 0..filled {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&(i as u64).to_le_bytes());
            leaves.push(h);
        }

        let mut via_set_leaf = MerkleTree::new(depth);
        for (i, leaf) in leaves.iter().enumerate() {
            via_set_leaf.set_leaf(i, *leaf);
        }

        let via_bulk = MerkleTree::from_filled_leaves(depth, &leaves);
        assert_eq!(via_set_leaf.root(), via_bulk.root());

        for idx in [0usize, 1, 2, filled - 1, capacity - 1] {
            assert_eq!(via_set_leaf.open(idx), via_bulk.open(idx));
        }
    }

    #[test]
    fn set_leaves_contiguous_matches_set_leaf() {
        let depth: u8 = 8;
        let capacity = 1usize << (depth as usize);

        // Prepare a base tree with some initial leaves.
        let prefix_len = 50usize;
        let mut base_leaves: Vec<Hash32> = Vec::with_capacity(prefix_len);
        for i in 0..prefix_len {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&(i as u64 + 1000).to_le_bytes());
            base_leaves.push(h);
        }

        // New contiguous values to set at positions [prefix_len, prefix_len + count).
        let count = 100usize;
        assert!(prefix_len + count <= capacity);
        let mut values: Vec<Hash32> = Vec::with_capacity(count);
        for i in 0..count {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&((prefix_len + i) as u64).to_le_bytes());
            values.push(h);
        }

        // Reference: set_leaf one by one.
        let mut via_set_leaf = MerkleTree::from_filled_leaves(depth, &base_leaves);
        for (i, val) in values.iter().enumerate() {
            via_set_leaf.set_leaf(prefix_len + i, *val);
        }

        // Bulk: set_leaves_contiguous.
        let mut via_bulk = MerkleTree::from_filled_leaves(depth, &base_leaves);
        via_bulk.set_leaves_contiguous(prefix_len, &values);

        assert_eq!(via_set_leaf.root(), via_bulk.root());
        for idx in [
            0usize,
            prefix_len - 1,
            prefix_len,
            prefix_len + count - 1,
            capacity - 1,
        ] {
            assert_eq!(
                via_set_leaf.open(idx),
                via_bulk.open(idx),
                "opening mismatch at index {}",
                idx
            );
        }
    }

    #[test]
    fn set_leaves_contiguous_empty_is_noop() {
        let depth: u8 = 4;
        let tree_a = MerkleTree::new(depth);
        let mut tree_b = MerkleTree::new(depth);
        tree_b.set_leaves_contiguous(0, &[]);
        assert_eq!(tree_a.root(), tree_b.root());
    }
}
