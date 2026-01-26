//! Offline benchmark for the "server-like" commitment tree rebuild path (no rollup node required).
//!
//! This aims to approximate what `mcp-external` does on a cold cache when it needs to rebuild the
//! Midnight Privacy commitment tree from a `(next_position, notes)` snapshot:
//! - materialize a `filled_leaves` prefix vector
//! - build a `commitment -> position` map
//! - rebuild the full `MerkleTree` of depth `depth` via `MerkleTree::from_filled_leaves`
//! - perform a small number of `open()` lookups to mimic transfer usage
//!
//! Run (debug):
//! `cargo test -p mcp-external --test commitment_tree_sync_benchmark -- --ignored --nocapture`
//!
//! Run (release, recommended):
//! `cargo test --release -p mcp-external --test commitment_tree_sync_benchmark -- --ignored --nocapture`
//!
//! Env vars:
//! - `MERKLE_SYNC_BENCH_DEPTH` (default 16 in release, 12 in debug)
//! - `MERKLE_SYNC_BENCH_FILLED_LEAVES` (default 4120; clamped to `2^depth`)
//! - `MERKLE_SYNC_BENCH_OPENINGS` (default 4)
//! - `MERKLE_SYNC_BENCH_MIN_SECS` (default 2 in release, 0.5 in debug)
//!
//! Notes:
//! - This is a micro-benchmark: it does *not* model network/JSON overhead from the rollup REST API.
//! - The loop runs until `MIN_SECS` to make it easy to get multi-second runs for comparing changes.

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

fn generate_commitment(position: u64) -> Hash32 {
    // Deterministic "mock commitments": encode position into first 8 bytes (little-endian).
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&position.to_le_bytes());
    h
}

fn generate_notes(filled: usize) -> Vec<(u64, Hash32)> {
    let mut out = Vec::with_capacity(filled);
    for pos in 0..filled {
        let p = pos as u64;
        out.push((p, generate_commitment(p)));
    }
    out
}

#[test]
#[ignore = "manual benchmark"]
fn bench_commitment_tree_rebuild_like_server() {
    // Debug builds can be very slow for Poseidon hashing; use smaller defaults to keep iteration sane.
    let default_depth = if cfg!(debug_assertions) { 12 } else { 16 };
    let default_min_secs = if cfg!(debug_assertions) { 0.5 } else { 2.0 };
    if cfg!(debug_assertions) {
        eprintln!("NOTE: debug build detected; use `--release` for realistic timings.");
    }

    let depth = env_u8("MERKLE_SYNC_BENCH_DEPTH", default_depth);
    assert!(depth <= 20, "depth too large for a test benchmark");
    let capacity = 1usize << (depth as usize);

    let filled_raw = env_usize("MERKLE_SYNC_BENCH_FILLED_LEAVES", 4120);
    let filled = filled_raw.min(capacity);
    let openings = env_usize("MERKLE_SYNC_BENCH_OPENINGS", 4).max(1);
    let min_secs = env_f64("MERKLE_SYNC_BENCH_MIN_SECS", default_min_secs).max(0.0);
    let min_duration = Duration::from_secs_f64(min_secs);

    // Pre-generate deterministic notes once (this is not what we're benchmarking).
    let notes = generate_notes(filled);

    eprintln!("--- commitment tree rebuild benchmark (server-like, offline) ---");
    eprintln!("depth={} capacity={} filled_leaves={}", depth, capacity, filled);
    eprintln!(
        "openings_per_iter={} min_secs={}",
        openings,
        min_duration.as_secs_f64()
    );

    let bench_started = Instant::now();
    let mut iters: u64 = 0;
    let mut prep_total = Duration::ZERO;
    let mut build_total = Duration::ZERO;
    let mut open_total = Duration::ZERO;

    while bench_started.elapsed() < min_duration {
        // (A) Materialize prefix leaves + build commitment->pos map.
        let prep_started = Instant::now();
        let mut leaves: Vec<Hash32> = vec![[0u8; 32]; filled];
        let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::with_capacity(filled.saturating_mul(2));
        for (pos, cm) in &notes {
            leaves[*pos as usize] = *cm;
            pos_by_cm.insert(*cm, *pos);
        }
        prep_total += prep_started.elapsed();

        // (B) Rebuild the full tree.
        let build_started = Instant::now();
        let tree = MerkleTree::from_filled_leaves(depth, &leaves);
        let root = tree.root();
        black_box(root);
        build_total += build_started.elapsed();

        // (C) Mimic transfer usage: a few lookups + openings.
        let open_started = Instant::now();
        if filled > 0 {
            for k in 0..openings {
                // Sample a spread of positions within the filled prefix.
                let idx = (k * (filled - 1)) / openings;
                let cm = leaves[idx];
                let pos = *pos_by_cm.get(&cm).expect("commitment should exist in map");
                black_box(tree.open(pos as usize));
            }
        }
        open_total += open_started.elapsed();

        iters += 1;
    }

    let elapsed = bench_started.elapsed();
    let iters_f = iters.max(1) as f64;
    let avg_prep_ms = prep_total.as_secs_f64() * 1000.0 / iters_f;
    let avg_build_ms = build_total.as_secs_f64() * 1000.0 / iters_f;
    let avg_open_ms = open_total.as_secs_f64() * 1000.0 / iters_f;

    eprintln!("iters={} total_s={:.3}", iters, elapsed.as_secs_f64());
    eprintln!(
        "avg_ms: prep={:.2} build={:.2} open={:.2} total={:.2}",
        avg_prep_ms,
        avg_build_ms,
        avg_open_ms,
        avg_prep_ms + avg_build_ms + avg_open_ms
    );
}
