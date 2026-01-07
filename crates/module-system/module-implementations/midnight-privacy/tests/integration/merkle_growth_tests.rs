//! Tests for MerkleTree auto-growth functionality.
//!
//! These tests verify that the commitment and nullifier trees grow correctly
//! when they reach capacity, implementing the Verdict-style doubling technique.

use midnight_privacy::{mt_combine, MerkleTree};

#[test]
fn tree_grows_and_preserves_existing_leaves() {
    // Start with a tiny tree to keep the test fast: depth=2 => 4 leaves.
    let mut tree = MerkleTree::new(2);

    // Fill the initial capacity.
    for i in 0..4 {
        let mut leaf = [0u8; 32];
        leaf[0] = i as u8;
        tree.set_leaf(i, leaf);
    }

    let old_depth = tree.depth();
    let old_len = tree.len();
    let old_root = tree.root();

    // Now we need space for at least 5 leaves -> should trigger one growth step.
    tree.grow_to_fit(5);

    assert_eq!(tree.depth(), old_depth + 1);
    assert_eq!(tree.len(), old_len * 2);

    // Existing leaves are unchanged.
    for i in 0..4 {
        let l = tree.leaf(i);
        assert_eq!(l[0], i as u8);
    }

    // The old root should now be the left child of the new root.
    // Reconstruct the level-1 sibling for leaf 0 and follow the path upwards
    // to ensure we reach the new root.
    let path = tree.open(0);
    assert_eq!(path.len(), tree.depth() as usize);

    // Just sanity-check: first sibling is leaf 1, etc.
    assert_eq!(path[0][0], 1u8);

    // Root changed (tree became deeper) but is a valid Merkle root
    // for the same leaves + extra zeros.
    assert_ne!(tree.root(), old_root);
}

#[test]
fn tree_grows_multiple_times() {
    // Start with depth=1 (2 leaves)
    let mut tree = MerkleTree::new(1);
    assert_eq!(tree.len(), 2);
    assert_eq!(tree.depth(), 1);

    // Set initial leaves
    let mut leaf0 = [0u8; 32];
    leaf0[0] = 0xAA;
    let mut leaf1 = [0u8; 32];
    leaf1[0] = 0xBB;
    tree.set_leaf(0, leaf0);
    tree.set_leaf(1, leaf1);

    // Grow to fit 10 leaves (requires depth=4, which has 16 leaves)
    tree.grow_to_fit(10);

    assert!(tree.len() >= 10);
    assert_eq!(tree.depth(), 4); // 2^4 = 16 >= 10

    // Original leaves preserved
    assert_eq!(tree.leaf(0)[0], 0xAA);
    assert_eq!(tree.leaf(1)[0], 0xBB);

    // New positions are zero leaves
    assert_eq!(tree.leaf(5), [0u8; 32]);
    assert_eq!(tree.leaf(15), [0u8; 32]);
}

#[test]
fn tree_no_growth_when_not_needed() {
    let mut tree = MerkleTree::new(4); // 16 leaves
    let original_depth = tree.depth();
    let original_root = tree.root();

    // Request fits within current capacity
    tree.grow_to_fit(10);

    assert_eq!(tree.depth(), original_depth);
    assert_eq!(tree.root(), original_root);
}

#[test]
fn grown_tree_produces_valid_merkle_proofs() {
    let mut tree = MerkleTree::new(2); // 4 leaves

    // Set leaves
    for i in 0..4 {
        let mut leaf = [0u8; 32];
        leaf[0] = (i + 1) as u8;
        tree.set_leaf(i, leaf);
    }

    // Grow and add more leaves
    tree.grow_to_fit(8);
    for i in 4..8 {
        let mut leaf = [0u8; 32];
        leaf[0] = (i + 1) as u8;
        tree.set_leaf(i, leaf);
    }

    // Verify paths for all leaves recompute to root
    let root = tree.root();
    for i in 0..8 {
        let leaf = tree.leaf(i);
        let path = tree.open(i);

        // Manually recompute root from leaf + path
        let mut cur = leaf;
        let mut idx = i;
        for (lvl, sib) in path.iter().enumerate() {
            cur = if (idx & 1) == 0 {
                mt_combine(lvl as u8, &cur, sib)
            } else {
                mt_combine(lvl as u8, sib, &cur)
            };
            idx >>= 1;
        }

        assert_eq!(cur, root, "Path verification failed for leaf {}", i);
    }
}

#[test]
fn empty_tree_root_consistency() {
    // Verify that an empty tree at different depths has the expected structure
    let tree2 = MerkleTree::new(2);
    let tree3 = MerkleTree::new(3);

    // Tree3's root should equal mt_combine(2, tree2.root(), tree2.root())
    // because both subtrees are all-zero
    let expected_root3 = mt_combine(2, &tree2.root(), &tree2.root());
    assert_eq!(tree3.root(), expected_root3);
}

/// Simulates the exact scenario in `add_commitment` where:
/// - Tree is at capacity (next_position == tree.len())
/// - A new commitment needs to be added
/// - Tree should auto-grow and accept the new commitment
///
/// This test mirrors the code path:
/// ```ignore
/// if position >= tree.len() as u64 {
///     tree.grow_to_fit((position + 1) as usize);
/// }
/// tree.set_leaf(position as usize, commitment);
/// ```
#[test]
fn simulates_add_commitment_at_capacity() {
    // Start with a tiny tree: depth=2 => 4 leaves (simulates tree_depth=2 in config)
    let mut tree = MerkleTree::new(2);
    let mut next_position: u64 = 0;

    // Simulate 4 deposits filling the tree to capacity
    for i in 0..4u64 {
        let mut commitment = [0u8; 32];
        commitment[0] = (i + 1) as u8;
        commitment[31] = 0xFF; // marker to identify real commitments

        // This is what add_commitment does:
        assert!(
            next_position < tree.len() as u64,
            "Tree should have capacity"
        );
        tree.set_leaf(next_position as usize, commitment);
        next_position += 1;
    }

    // Now tree is FULL: next_position (4) == tree.len() (4)
    assert_eq!(next_position, 4);
    assert_eq!(tree.len(), 4);
    assert_eq!(
        next_position as usize,
        tree.len(),
        "Tree should be at capacity"
    );

    // Record state before growth
    let pre_growth_depth = tree.depth();
    let pre_growth_root = tree.root();

    // Simulate the 5th deposit that would trigger growth
    let mut commitment_5 = [0u8; 32];
    commitment_5[0] = 5;
    commitment_5[31] = 0xFF;

    // This is the exact check from add_commitment:
    // if position >= tree.len() as u64 { tree.grow_to_fit((position + 1) as usize); }
    if next_position >= tree.len() as u64 {
        tree.grow_to_fit((next_position + 1) as usize);
    }

    // Verify tree grew
    assert_eq!(
        tree.depth(),
        pre_growth_depth + 1,
        "Tree should have grown by 1 level"
    );
    assert_eq!(tree.len(), 8, "Tree capacity should have doubled");
    assert_ne!(
        tree.root(),
        pre_growth_root,
        "Root should have changed after growth"
    );

    // Now we can add the commitment
    tree.set_leaf(next_position as usize, commitment_5);
    let final_position = next_position + 1;
    assert_eq!(final_position, 5, "Should have 5 commitments");

    // Verify all commitments are intact (positions 0..final_position)
    for i in 0..final_position {
        let leaf = tree.leaf(i as usize);
        assert_eq!(
            leaf[0],
            (i + 1) as u8,
            "Commitment {} should be preserved",
            i
        );
        assert_eq!(
            leaf[31], 0xFF,
            "Commitment {} marker should be preserved",
            i
        );
    }

    // Positions 5, 6, 7 should be zero (unused capacity)
    for i in 5..8 {
        assert_eq!(tree.leaf(i), [0u8; 32], "Position {} should be zero", i);
    }

    // Verify Merkle proofs still work for all commitments
    let root = tree.root();
    for i in 0..5 {
        let leaf = tree.leaf(i);
        let path = tree.open(i);

        let mut cur = leaf;
        let mut idx = i;
        for (lvl, sib) in path.iter().enumerate() {
            cur = if (idx & 1) == 0 {
                mt_combine(lvl as u8, &cur, sib)
            } else {
                mt_combine(lvl as u8, sib, &cur)
            };
            idx >>= 1;
        }
        assert_eq!(cur, root, "Merkle proof failed for commitment {}", i);
    }
}

/// Simulates the scenario where a tree grows multiple times during heavy usage.
/// This mirrors a production scenario where many deposits/transfers accumulate
/// and the tree needs to grow several times.
#[test]
fn simulates_heavy_usage_multiple_growths() {
    // Start with depth=1 (2 leaves) - very small for testing
    let mut tree = MerkleTree::new(1);
    let mut next_position: u64 = 0;

    // Simulate 20 deposits, which requires growing from 2 -> 4 -> 8 -> 16 -> 32 leaves
    // (depth 1 -> 2 -> 3 -> 4 -> 5)
    let target_deposits = 20u64;

    for i in 0..target_deposits {
        let mut commitment = [0u8; 32];
        // Use unique commitment values
        commitment[0] = (i & 0xFF) as u8;
        commitment[1] = ((i >> 8) & 0xFF) as u8;
        commitment[31] = 0xAB; // marker

        // Exact add_commitment logic:
        if next_position >= tree.len() as u64 {
            let old_depth = tree.depth();
            tree.grow_to_fit((next_position + 1) as usize);
            println!(
                "Tree grew: depth {} -> {}, capacity {} -> {} (at position {})",
                old_depth,
                tree.depth(),
                1usize << old_depth,
                tree.len(),
                next_position
            );
        }

        tree.set_leaf(next_position as usize, commitment);
        next_position += 1;
    }

    // Final state checks
    assert_eq!(next_position, target_deposits);
    assert!(
        tree.len() >= target_deposits as usize,
        "Tree should have enough capacity"
    );
    assert_eq!(
        tree.depth(),
        5,
        "Should need depth 5 for 20 leaves (2^5 = 32)"
    );

    // Verify all 20 commitments are intact
    for i in 0..target_deposits {
        let leaf = tree.leaf(i as usize);
        assert_eq!(leaf[0], (i & 0xFF) as u8);
        assert_eq!(leaf[1], ((i >> 8) & 0xFF) as u8);
        assert_eq!(leaf[31], 0xAB);
    }

    // Verify Merkle proofs for a sample of commitments
    let root = tree.root();
    for i in [0, 1, 2, 10, 15, 19] {
        let leaf = tree.leaf(i);
        let path = tree.open(i);

        let mut cur = leaf;
        let mut idx = i;
        for (lvl, sib) in path.iter().enumerate() {
            cur = if (idx & 1) == 0 {
                mt_combine(lvl as u8, &cur, sib)
            } else {
                mt_combine(lvl as u8, sib, &cur)
            };
            idx >>= 1;
        }
        assert_eq!(cur, root, "Merkle proof failed for commitment {}", i);
    }
}

/// Tests that the nullifier tree (which uses the same MerkleTree type)
/// grows correctly in the same manner as the commitment tree.
#[test]
fn simulates_nullifier_tree_growth() {
    // Nullifier tree starts with same depth as commitment tree
    let mut nf_tree = MerkleTree::new(2); // 4 leaves
    let mut next_nf_position: u64 = 0;

    // Simulate spending 10 notes (10 nullifiers)
    // Use 0xAF as a marker byte for nullifiers
    const NF_MARKER: u8 = 0xAF;
    for i in 0..10u64 {
        // Create a unique nullifier (in production this is a hash)
        let mut nullifier = [0u8; 32];
        nullifier[0] = NF_MARKER.wrapping_add(i as u8);
        nullifier[1] = (i >> 8) as u8;

        // Exact append_nullifier logic:
        if next_nf_position >= nf_tree.len() as u64 {
            nf_tree.grow_to_fit((next_nf_position + 1) as usize);
        }
        nf_tree.set_leaf(next_nf_position as usize, nullifier);
        next_nf_position += 1;
    }

    // Should have grown: 4 -> 8 -> 16 to fit 10 nullifiers
    assert_eq!(next_nf_position, 10);
    assert!(nf_tree.len() >= 10);
    assert_eq!(nf_tree.depth(), 4); // 2^4 = 16 >= 10

    // All nullifiers preserved
    for i in 0..10u64 {
        let leaf = nf_tree.leaf(i as usize);
        assert_eq!(leaf[0], NF_MARKER.wrapping_add(i as u8));
    }
}

/// Test that MerkleTree::new() panics if depth exceeds MAX_TREE_DEPTH
#[test]
#[should_panic(expected = "exceeds MAX_TREE_DEPTH")]
fn new_tree_rejects_excessive_depth() {
    // MAX_TREE_DEPTH is 63, so 64 should panic
    let _ = MerkleTree::new(64);
}
