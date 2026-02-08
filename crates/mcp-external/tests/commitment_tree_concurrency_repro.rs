//! Offline concurrency reproduction tests for commitment-tree sync behavior.
//!
//! This test harness models the key DB sync path used by `mcp-external`:
//! - cursor-based deltas
//! - mixed `rollup_height` trigger for snapshot rebuild
//! - global sync lock + in-lock rebuild work
//!
//! It is intentionally offline (no rollup/indexer required) but exercises
//! concurrency and large note volumes to reproduce "batch 2 / high concurrency
//! starts rebuilding" behavior.
//!
//! Manual heavy run (release, recommended):
//! `cargo test --release -p mcp-external --test commitment_tree_concurrency_repro -- --ignored --nocapture`

use midnight_privacy::{Hash32, MerkleTree};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Copy)]
struct NoteRow {
    commitment: Hash32,
    rollup_height: Option<u64>,
}

#[derive(Clone)]
struct NoteBatch {
    upto_event_id: i64,
    notes: Vec<NoteRow>,
}

struct ScriptedSource {
    rows: Vec<NoteRow>,
    visible_rows: Mutex<usize>,
}

impl ScriptedSource {
    fn new(rows: Vec<NoteRow>) -> Self {
        Self {
            rows,
            visible_rows: Mutex::new(0),
        }
    }

    async fn publish_more(&self, count: usize) -> usize {
        let mut visible = self.visible_rows.lock().await;
        let target = (*visible).saturating_add(count).min(self.rows.len());
        *visible = target;
        target
    }

    async fn current_visible(&self) -> usize {
        *self.visible_rows.lock().await
    }

    async fn delta_since(&self, last_event_id: i64) -> NoteBatch {
        let visible = self.current_visible().await;
        let start = usize::try_from(last_event_id.max(0))
            .unwrap_or(0)
            .min(visible);
        let notes = self.rows[start..visible].to_vec();
        NoteBatch {
            upto_event_id: visible as i64,
            notes,
        }
    }

    async fn snapshot(&self) -> NoteBatch {
        let visible = self.current_visible().await;
        let notes = self.rows[..visible].to_vec();
        NoteBatch {
            upto_event_id: visible as i64,
            notes,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct SyncOutcome {
    rebuilds: usize,
    incrementals: usize,
}

struct ModelState {
    tree: MerkleTree,
    pos_by_cm: HashMap<Hash32, u64>,
    next_position: u64,
    indexer_last_event_id: i64,
    total_rebuilds: usize,
    total_incrementals: usize,
}

impl ModelState {
    fn new(default_depth: u8) -> Self {
        Self {
            tree: MerkleTree::new(default_depth),
            pos_by_cm: HashMap::new(),
            next_position: 0,
            indexer_last_event_id: 0,
            total_rebuilds: 0,
            total_incrementals: 0,
        }
    }
}

struct ModelSyncer {
    default_depth: u8,
    state: RwLock<ModelState>,
    sync_lock: Mutex<()>,
}

impl ModelSyncer {
    fn new(default_depth: u8) -> Self {
        Self {
            default_depth,
            state: RwLock::new(ModelState::new(default_depth)),
            sync_lock: Mutex::new(()),
        }
    }

    async fn sync_to_visible(&self, source: &ScriptedSource) -> SyncOutcome {
        let start_event_id = { self.state.read().await.indexer_last_event_id };
        let delta = source.delta_since(start_event_id).await;
        if delta.upto_event_id == start_event_id || delta.notes.is_empty() {
            return SyncOutcome::default();
        }

        let has_any_rollup_height = delta.notes.iter().any(|n| n.rollup_height.is_some());
        let has_any_without_rollup_height = delta.notes.iter().any(|n| n.rollup_height.is_none());
        let mut ordered_delta_notes = delta.notes;

        let mut snapshot_rebuild: Option<(Vec<NoteRow>, i64)> = None;
        if has_any_rollup_height && has_any_without_rollup_height {
            // Mirrors the production "mixed metadata -> snapshot rebuild" trigger.
            let snapshot = source.snapshot().await;
            let ordered = reorder_snapshot_notes_for_db_sync(snapshot.notes);
            snapshot_rebuild = Some((ordered, snapshot.upto_event_id));
        } else if has_any_rollup_height {
            sort_rollup_height_commitments(&mut ordered_delta_notes);
        }

        let _guard = self.sync_lock.lock().await;
        let mut st = self.state.write().await;

        // Another task may have advanced while we were fetching.
        if st.indexer_last_event_id != start_event_id {
            if st.indexer_last_event_id >= delta.upto_event_id {
                return SyncOutcome::default();
            }
            return SyncOutcome::default();
        }

        if let Some((ordered_notes, snapshot_upto_event_id)) = snapshot_rebuild {
            // Intentionally expensive in-lock rebuild to mirror current production behavior.
            let expected_next = ordered_notes.len() as u64;
            let depth = required_depth_for_next_position(expected_next).max(self.default_depth);
            let mut tree = MerkleTree::new(depth);
            if expected_next > tree.len() as u64 {
                tree.grow_to_fit(expected_next as usize);
            }

            let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::with_capacity(ordered_notes.len());
            for (pos, note) in ordered_notes.into_iter().enumerate() {
                tree.set_leaf(pos, note.commitment);
                pos_by_cm.insert(note.commitment, pos as u64);
            }

            st.tree = tree;
            st.pos_by_cm = pos_by_cm;
            st.next_position = expected_next;
            st.indexer_last_event_id = snapshot_upto_event_id;
            st.total_rebuilds += 1;
            return SyncOutcome {
                rebuilds: 1,
                incrementals: 0,
            };
        }

        for note in &ordered_delta_notes {
            let cm = note.commitment;
            let pos = st.next_position;
            if pos as usize >= st.tree.len() {
                st.tree.grow_to_fit(pos as usize + 1);
            }
            st.tree.set_leaf(pos as usize, cm);
            st.pos_by_cm.insert(cm, pos);
            st.next_position = pos + 1;
        }
        st.indexer_last_event_id = delta.upto_event_id;
        st.total_incrementals += 1;
        SyncOutcome {
            rebuilds: 0,
            incrementals: 1,
        }
    }

    async fn state_snapshot(&self) -> (u64, u8, Hash32, usize, usize) {
        let st = self.state.read().await;
        (
            st.next_position,
            st.tree.depth(),
            st.tree.root(),
            st.total_rebuilds,
            st.total_incrementals,
        )
    }
}

fn required_depth_for_next_position(next_position: u64) -> u8 {
    if next_position <= 1 {
        return 0;
    }
    let depth_u32 = 64u32 - (next_position - 1).leading_zeros();
    u8::try_from(depth_u32).expect("depth_u32 <= 64")
}

fn sort_rollup_height_commitments(notes: &mut [NoteRow]) {
    notes.sort_unstable_by(|a, b| {
        let left_h = a
            .rollup_height
            .expect("sort_rollup_height_commitments requires rollup_height");
        let right_h = b
            .rollup_height
            .expect("sort_rollup_height_commitments requires rollup_height");
        left_h
            .cmp(&right_h)
            .then_with(|| a.commitment.cmp(&b.commitment))
    });
}

fn reorder_snapshot_notes_for_db_sync(mut notes: Vec<NoteRow>) -> Vec<NoteRow> {
    let first_with_height = notes.iter().position(|n| n.rollup_height.is_some());
    let Some(first_with_height) = first_with_height else {
        // Legacy append order only.
        return notes;
    };

    let has_any_without_height = notes.iter().any(|n| n.rollup_height.is_none());
    if !has_any_without_height {
        // Deterministic ordering when metadata exists for all rows.
        sort_rollup_height_commitments(&mut notes);
        return notes;
    }

    // Mixed history: keep legacy prefix order; sort suffix with metadata.
    if notes[first_with_height..]
        .iter()
        .any(|n| n.rollup_height.is_none())
    {
        // Interleaved mixed metadata is ambiguous in production too. Keep raw order to
        // avoid introducing synthetic behavior in this test harness.
        return notes;
    }
    sort_rollup_height_commitments(&mut notes[first_with_height..]);
    notes
}

fn commitment_for(position: u64) -> Hash32 {
    let mut cm = [0u8; 32];
    cm[..8].copy_from_slice(&position.to_le_bytes());
    cm
}

fn build_mixed_stream(total: usize, legacy_prefix: usize) -> Vec<NoteRow> {
    let mut out = Vec::with_capacity(total);
    for i in 0..total {
        let rollup_height = if i < legacy_prefix {
            None
        } else {
            Some((i - legacy_prefix) as u64)
        };
        out.push(NoteRow {
            commitment: commitment_for(i as u64),
            rollup_height,
        });
    }
    out
}

fn rebuild_root_from_visible(rows: &[NoteRow], visible: usize, default_depth: u8) -> (Hash32, u8) {
    let ordered_rows =
        reorder_snapshot_notes_for_db_sync(rows.iter().copied().take(visible).collect());
    let leaves: Vec<Hash32> = ordered_rows.iter().map(|r| r.commitment).collect();
    let depth = required_depth_for_next_position(visible as u64).max(default_depth);
    let tree = MerkleTree::from_filled_leaves(depth, &leaves);
    (tree.root(), tree.depth())
}

#[derive(Default, Clone, Copy)]
struct WorkloadMetrics {
    first_rebuild_round: Option<usize>,
    rebuilds: usize,
    incrementals: usize,
    elapsed: Duration,
}

async fn run_wallet_workload(
    wallets: usize,
    tx_rounds: usize,
    notes_per_wallet_per_round: usize,
    stream_rows: &[NoteRow],
) -> WorkloadMetrics {
    let source = Arc::new(ScriptedSource::new(stream_rows.to_vec()));
    let syncer = Arc::new(ModelSyncer::new(16));
    let mut metrics = WorkloadMetrics::default();
    let started = Instant::now();

    for round in 1..=tx_rounds {
        let to_publish = wallets.saturating_mul(notes_per_wallet_per_round);
        source.publish_more(to_publish).await;

        let mut handles = Vec::with_capacity(wallets);
        for _ in 0..wallets {
            let source_cloned = Arc::clone(&source);
            let syncer_cloned = Arc::clone(&syncer);
            handles.push(tokio::spawn(async move {
                syncer_cloned.sync_to_visible(source_cloned.as_ref()).await
            }));
        }

        let mut rebuild_happened_this_round = false;
        for handle in handles {
            let outcome = handle.await.expect("sync task should not panic");
            if outcome.rebuilds > 0 {
                rebuild_happened_this_round = true;
            }
            metrics.rebuilds += outcome.rebuilds;
            metrics.incrementals += outcome.incrementals;
        }
        if rebuild_happened_this_round && metrics.first_rebuild_round.is_none() {
            metrics.first_rebuild_round = Some(round);
        }
    }

    metrics.elapsed = started.elapsed();

    // Correctness check: model sync state must match full rebuild of currently visible rows.
    let visible = source.current_visible().await;
    let (expected_root, expected_depth) = rebuild_root_from_visible(stream_rows, visible, 16);
    let (next_position, tree_depth, root, _rebuilds, _incrementals) = syncer.state_snapshot().await;
    assert_eq!(next_position as usize, visible, "next_position mismatch");
    assert_eq!(tree_depth, expected_depth, "tree depth mismatch");
    assert_eq!(root, expected_root, "tree root mismatch");

    metrics
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reproduces_concurrency_cutover_pattern_fast() {
    // Fast default profile for regular test runs.
    // Costly 10k+ behavior lives in `costly_concurrency_rebuild_pressure_10k_plus_notes`.
    let total_rows = 12_000usize;
    let legacy_prefix = 260usize;
    let tx_rounds = 6usize;
    let notes_per_wallet_per_round = 20usize;
    let stream_rows = build_mixed_stream(total_rows, legacy_prefix);

    let two_wallets =
        run_wallet_workload(2, tx_rounds, notes_per_wallet_per_round, &stream_rows).await;

    let four_wallets =
        run_wallet_workload(4, tx_rounds, notes_per_wallet_per_round, &stream_rows).await;

    // 2 wallets * 6 rounds * 20 = 240 notes: should remain in legacy-only range.
    assert_eq!(
        two_wallets.first_rebuild_round, None,
        "2-wallet workload should not hit mixed-metadata rebuild in configured rounds"
    );

    // 4 wallets * 6 rounds * 20 = 480 notes. The 260 cutover is crossed mid-run, so
    // a mixed-metadata rebuild should appear before the end.
    assert!(
        four_wallets.first_rebuild_round.is_some(),
        "4-wallet workload should hit mixed-metadata rebuild"
    );
    assert!(
        four_wallets.first_rebuild_round.unwrap_or(usize::MAX) <= 4,
        "rebuild should appear in early-mid rounds, got {:?}",
        four_wallets.first_rebuild_round
    );

    eprintln!(
        "2 wallets: first_rebuild={:?}, rebuilds={}, incrementals={}, elapsed_ms={}",
        two_wallets.first_rebuild_round,
        two_wallets.rebuilds,
        two_wallets.incrementals,
        two_wallets.elapsed.as_millis()
    );
    eprintln!(
        "4 wallets: first_rebuild={:?}, rebuilds={}, incrementals={}, elapsed_ms={}",
        four_wallets.first_rebuild_round,
        four_wallets.rebuilds,
        four_wallets.incrementals,
        four_wallets.elapsed.as_millis()
    );
}
