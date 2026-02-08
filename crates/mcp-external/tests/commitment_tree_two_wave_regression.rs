//! Offline two-wave regression tests for commitment-tree behavior.
//!
//! These tests do not require a running rollup/indexer. They focus on the
//! "batch 2 got expensive" symptom by checking:
//! - incremental append correctness vs full rebuild
//! - correctness across a depth-boundary crossing
//! - optional manual timing regression for wave2 append vs full rebuild
//!
//! Manual benchmark run (release, recommended):
//! `cargo test --release -p mcp-external --test commitment_tree_two_wave_regression -- --ignored --nocapture`

use midnight_privacy::{Hash32, MerkleTree};
use std::collections::HashMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

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

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(default)
}

fn required_depth_for_next_position(next_position: u64) -> u8 {
    if next_position <= 1 {
        return 0;
    }
    let depth_u32 = 64u32 - (next_position - 1).leading_zeros();
    u8::try_from(depth_u32).expect("depth_u32 <= 64")
}

fn commitment_for(position: u64) -> Hash32 {
    let mut cm = [0u8; 32];
    cm[..8].copy_from_slice(&position.to_le_bytes());
    cm
}

fn commitments_range(start: u64, count: usize) -> Vec<Hash32> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(commitment_for(start + i as u64));
    }
    out
}

fn apply_incremental_batch(
    tree: &mut MerkleTree,
    pos_by_cm: &mut HashMap<Hash32, u64>,
    start_pos: u64,
    cms: &[Hash32],
) {
    let final_next = start_pos as usize + cms.len();
    if final_next > tree.len() {
        tree.grow_to_fit(final_next);
    }

    for (offset, cm) in cms.iter().copied().enumerate() {
        let pos = start_pos as usize + offset;
        tree.set_leaf(pos, cm);
        pos_by_cm.insert(cm, pos as u64);
    }
}

fn full_rebuild_tree_and_map(depth: u8, cms: &[Hash32]) -> (MerkleTree, HashMap<Hash32, u64>) {
    let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::with_capacity(cms.len().saturating_mul(2));
    for (pos, cm) in cms.iter().copied().enumerate() {
        pos_by_cm.insert(cm, pos as u64);
    }
    let tree = MerkleTree::from_filled_leaves(depth, cms);
    (tree, pos_by_cm)
}

#[test]
fn two_wave_incremental_matches_full_rebuild() {
    // Keep default runtime low for regular CI loops.
    let depth: u8 = 10;
    let wave1 = 320usize;
    let wave2 = 160usize;
    let total = wave1 + wave2;
    assert!(
        total <= (1usize << depth),
        "test setup exceeds capacity for depth {}",
        depth
    );

    let mut tree = MerkleTree::new(depth);
    let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();

    let wave1_cms = commitments_range(0, wave1);
    apply_incremental_batch(&mut tree, &mut pos_by_cm, 0, &wave1_cms);
    let wave2_cms = commitments_range(wave1 as u64, wave2);
    apply_incremental_batch(&mut tree, &mut pos_by_cm, wave1 as u64, &wave2_cms);

    let all_cms = commitments_range(0, total);
    let (rebuilt_tree, rebuilt_map) = full_rebuild_tree_and_map(depth, &all_cms);

    assert_eq!(
        tree.root(),
        rebuilt_tree.root(),
        "incremental root must match full rebuild root"
    );

    let sample_indices = [0usize, 1, wave1 - 1, wave1, total / 2, total - 1];
    for idx in sample_indices {
        assert_eq!(
            tree.open(idx),
            rebuilt_tree.open(idx),
            "opening mismatch at index {}",
            idx
        );
    }

    let sample_commitments = [
        commitment_for(0),
        commitment_for(wave1 as u64),
        commitment_for((total - 1) as u64),
    ];
    for cm in sample_commitments {
        assert_eq!(
            pos_by_cm.get(&cm),
            rebuilt_map.get(&cm),
            "position map mismatch for commitment {}",
            hex::encode(cm)
        );
    }
}

#[test]
fn two_wave_incremental_matches_full_rebuild_across_depth_boundary() {
    let initial_depth: u8 = 8;
    let initial_capacity = 1usize << initial_depth;

    // Force wave2 to cross the depth boundary.
    let wave1 = initial_capacity - 16;
    let wave2 = 24usize;
    let total = wave1 + wave2;
    let required_depth = required_depth_for_next_position(total as u64).max(initial_depth);

    let mut tree = MerkleTree::new(initial_depth);
    let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();

    let wave1_cms = commitments_range(0, wave1);
    apply_incremental_batch(&mut tree, &mut pos_by_cm, 0, &wave1_cms);
    let wave2_cms = commitments_range(wave1 as u64, wave2);
    apply_incremental_batch(&mut tree, &mut pos_by_cm, wave1 as u64, &wave2_cms);

    let all_cms = commitments_range(0, total);
    let (rebuilt_tree, rebuilt_map) = full_rebuild_tree_and_map(required_depth, &all_cms);

    assert_eq!(
        tree.root(),
        rebuilt_tree.root(),
        "incremental root must match full rebuild root across depth boundary"
    );
    assert_eq!(
        tree.depth(),
        rebuilt_tree.depth(),
        "incremental tree depth must match rebuild depth"
    );
    assert_eq!(
        pos_by_cm.len(),
        rebuilt_map.len(),
        "position map length mismatch across depth boundary"
    );
}

#[test]
#[ignore = "manual benchmark"]
fn bench_wave2_append_vs_full_rebuild() {
    let default_depth = if cfg!(debug_assertions) { 12 } else { 16 };
    let default_wave1 = if cfg!(debug_assertions) {
        1_024
    } else {
        16_384
    };
    let default_wave2 = if cfg!(debug_assertions) { 256 } else { 2_048 };
    let default_min_secs = if cfg!(debug_assertions) { 0.5 } else { 2.0 };
    let default_max_ratio = if cfg!(debug_assertions) { 5.0 } else { 2.0 };

    let depth = env_u8("MCP_TREE_WAVE_BENCH_DEPTH", default_depth);
    assert!(depth <= 20, "depth too large for benchmark");
    let capacity = 1usize << depth as usize;

    let wave1 = env_usize("MCP_TREE_WAVE_BENCH_WAVE1", default_wave1)
        .min(capacity.saturating_sub(1))
        .max(1);
    let wave2 = env_usize("MCP_TREE_WAVE_BENCH_WAVE2", default_wave2)
        .min(capacity.saturating_sub(wave1))
        .max(1);
    let total = wave1 + wave2;

    let min_secs = env_f64("MCP_TREE_WAVE_BENCH_MIN_SECS", default_min_secs).max(0.0);
    let max_ratio = env_f64(
        "MCP_TREE_WAVE_BENCH_MAX_APPEND_TO_REBUILD_RATIO",
        default_max_ratio,
    );
    let min_duration = Duration::from_secs_f64(min_secs);

    let wave1_cms = commitments_range(0, wave1);
    let wave2_cms = commitments_range(wave1 as u64, wave2);
    let all_cms = commitments_range(0, total);

    let bench_started = Instant::now();
    let mut iters = 0u64;
    let mut append_total = Duration::ZERO;
    let mut rebuild_total = Duration::ZERO;

    while bench_started.elapsed() < min_duration {
        let mut tree = MerkleTree::from_filled_leaves(depth, &wave1_cms);
        let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::with_capacity(total.saturating_mul(2));
        for (pos, cm) in wave1_cms.iter().copied().enumerate() {
            pos_by_cm.insert(cm, pos as u64);
        }

        let append_started = Instant::now();
        apply_incremental_batch(&mut tree, &mut pos_by_cm, wave1 as u64, &wave2_cms);
        black_box(tree.root());
        black_box(pos_by_cm.len());
        append_total += append_started.elapsed();

        let rebuild_started = Instant::now();
        let (rebuilt_tree, rebuilt_map) = full_rebuild_tree_and_map(depth, &all_cms);
        black_box(rebuilt_tree.root());
        black_box(rebuilt_map.len());
        rebuild_total += rebuild_started.elapsed();

        iters += 1;
    }

    let iters_f64 = iters.max(1) as f64;
    let append_avg_ms = append_total.as_secs_f64() * 1000.0 / iters_f64;
    let rebuild_avg_ms = rebuild_total.as_secs_f64() * 1000.0 / iters_f64;
    let ratio = append_avg_ms / rebuild_avg_ms;

    eprintln!("--- wave2 append vs full rebuild (offline) ---");
    eprintln!(
        "depth={} wave1={} wave2={} total={} iterations={}",
        depth, wave1, wave2, total, iters
    );
    eprintln!(
        "avg_ms: wave2_append={:.2}, full_rebuild={:.2}, ratio={:.3}",
        append_avg_ms, rebuild_avg_ms, ratio
    );
    eprintln!("threshold ratio <= {:.3}", max_ratio);

    assert!(
        ratio <= max_ratio,
        "wave2 append path is too close to (or slower than) full rebuild: ratio={:.3} > {:.3}",
        ratio,
        max_ratio
    );
}
