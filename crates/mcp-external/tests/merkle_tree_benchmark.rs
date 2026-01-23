//! Offline benchmark for Merkle tree rebuild algorithms (no rollup node required).
//!
//! Run (debug):
//! `cargo test -p mcp-external --test merkle_tree_benchmark -- --ignored --nocapture`
//!
//! Run (release, recommended):
//! `cargo test --release -p mcp-external --test merkle_tree_benchmark -- --ignored --nocapture`
//!
//! Env vars:
//! - `MERKLE_BENCH_DEPTH` (default 16)
//! - `MERKLE_BENCH_FILLED_LEAVES` (default 65536, must be <= 2^depth)

use midnight_privacy::{mt_combine, Hash32, MerkleTree};
use std::hint::black_box;
use std::time::Instant;

fn env_u8(key: &str, default: u8) -> u8 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn generate_leaves(n: usize) -> Vec<Hash32> {
    // Deterministic, low-overhead "mock commitments": encode index into first 8 bytes.
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&(i as u64).to_le_bytes());
        out.push(h);
    }
    out
}

/// Old rebuild method: initialize `MerkleTree` then call `set_leaf()` for each filled leaf.
fn build_tree_via_set_leaf(depth: u8, filled_leaves: &[Hash32]) -> MerkleTree {
    let capacity = 1usize << (depth as usize);
    assert!(
        filled_leaves.len() <= capacity,
        "filled_leaves ({}) exceeds capacity ({}) for depth {}",
        filled_leaves.len(),
        capacity,
        depth
    );

    let mut tree = MerkleTree::new(depth);
    for (pos, cm) in filled_leaves.iter().enumerate() {
        tree.set_leaf(pos, *cm);
    }
    tree
}

/// Bottom-up rebuild method: build all tree levels in one pass (O(2^depth) hashes).
///
/// This mirrors `MerkleTree` semantics (fixed depth, zero leaves beyond `filled_leaves.len()`),
/// but avoids recomputing internal nodes `depth` times per leaf.
fn build_levels_bottom_up(depth: u8, filled_leaves: &[Hash32]) -> Vec<Vec<Hash32>> {
    let capacity = 1usize << (depth as usize);
    assert!(
        filled_leaves.len() <= capacity,
        "filled_leaves ({}) exceeds capacity ({}) for depth {}",
        filled_leaves.len(),
        capacity,
        depth
    );

    let mut levels: Vec<Vec<Hash32>> = Vec::with_capacity(depth as usize + 1);

    let mut level0 = vec![[0u8; 32]; capacity];
    level0[..filled_leaves.len()].copy_from_slice(filled_leaves);
    levels.push(level0);

    for lvl in 0..depth as usize {
        let prev = &levels[lvl];
        let mut next = Vec::with_capacity(prev.len() / 2);
        for i in 0..(prev.len() / 2) {
            let left = prev[i * 2];
            let right = prev[i * 2 + 1];
            next.push(mt_combine(lvl as u8, &left, &right));
        }
        levels.push(next);
    }

    debug_assert_eq!(levels.len(), depth as usize + 1);
    debug_assert_eq!(levels.last().unwrap().len(), 1);
    levels
}

fn open_from_levels(levels: &[Vec<Hash32>], index: usize) -> Vec<Hash32> {
    assert!(!levels.is_empty(), "levels must not be empty");
    let depth = levels.len() - 1;
    assert!(index < levels[0].len(), "index out of bounds");

    let mut idx = index;
    let mut path = Vec::with_capacity(depth);
    for lvl in 0..depth {
        let sib_idx = idx ^ 1;
        path.push(levels[lvl][sib_idx]);
        idx >>= 1;
    }
    path
}

#[test]
#[ignore = "manual benchmark"]
fn bench_merkle_tree_rebuild_offline() {
    // Debug builds are extremely slow for Poseidon hashing. Use a smaller default depth for
    // quick iteration, and override via env vars for realistic measurements.
    let default_depth = if cfg!(debug_assertions) { 12 } else { 16 };
    if cfg!(debug_assertions) {
        eprintln!("NOTE: debug build detected; use `--release` for realistic timings.");
    }

    let depth = env_u8("MERKLE_BENCH_DEPTH", default_depth);
    assert!(depth <= 20, "depth too large for a test benchmark");
    let capacity = 1usize << (depth as usize);
    let filled = env_usize("MERKLE_BENCH_FILLED_LEAVES", capacity).min(capacity);

    let leaves = generate_leaves(filled);

    eprintln!("--- merkle tree rebuild benchmark (offline) ---");
    eprintln!("depth={} capacity={} filled={}", depth, capacity, filled);

    let t0 = Instant::now();
    let tree = build_tree_via_set_leaf(depth, &leaves);
    let root_set_leaf = tree.root();
    black_box(root_set_leaf);
    eprintln!(
        "[old] MerkleTree::set_leaf() rebuild: {:.2} ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );

    let t1 = Instant::now();
    let levels = build_levels_bottom_up(depth, &leaves);
    let root_bottom_up = levels[depth as usize][0];
    black_box(root_bottom_up);
    eprintln!(
        "[new] bottom-up rebuild: {:.2} ms",
        t1.elapsed().as_secs_f64() * 1000.0
    );

    assert_eq!(
        root_set_leaf, root_bottom_up,
        "roots differ between algorithms"
    );

    // Quick correctness spot-check: openings match for a few indices.
    let test_indices = [
        0usize,
        1usize,
        2usize,
        3usize,
        filled.saturating_sub(1),
        capacity.saturating_sub(1),
        capacity / 2,
        capacity / 2 + 1,
    ];
    for idx in test_indices {
        if idx >= capacity {
            continue;
        }
        let a = tree.open(idx);
        let b = open_from_levels(&levels, idx);
        assert_eq!(a, b, "opening mismatch at index {}", idx);
    }
}
