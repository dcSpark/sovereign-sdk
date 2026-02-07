use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use midnight_privacy::{Hash32, MerkleTree, MAX_TREE_DEPTH};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::provider::{IndexerNoteCreated, Provider};

/// Default commitment-tree depth used by Midnight Privacy.
pub const DEFAULT_TREE_DEPTH: u8 = 16;

const NOTES_PAGE_LIMIT: usize = 1000;
const NOTES_EMPTY_PAGE_MAX_RETRIES: usize = 5;
const NOTES_EMPTY_PAGE_RETRY_DELAY_MS: u64 = 100;
const DEFAULT_SYNC_MAX_RETRIES: usize = 3;
const DEFAULT_SYNC_RETRY_DELAY_MS: u64 = 200;

const DEFAULT_POSITION_LOOKUP_MAX_RETRIES: usize = 50;
const DEFAULT_POSITION_LOOKUP_RETRY_DELAY_MS: u64 = 200;

const BACKGROUND_SYNC_INTERVAL_SECS: u64 = 1;
const BACKGROUND_SYNC_ERROR_BACKOFF_SECS: u64 = 3;

static GLOBAL_TREE_SYNCER: OnceLock<CommitmentTreeSyncer> = OnceLock::new();
static BACKGROUND_SYNC_STARTED: OnceLock<()> = OnceLock::new();
static SYNC_CONFIG: OnceLock<SyncConfig> = OnceLock::new();
static POSITION_LOOKUP_CONFIG: OnceLock<PositionLookupConfig> = OnceLock::new();

#[derive(Clone, Copy)]
struct SyncConfig {
    max_retries: usize,
    retry_delay: Duration,
}

#[derive(Clone, Copy)]
struct PositionLookupConfig {
    max_retries: usize,
    retry_delay: Duration,
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn use_index_db_tree_sync() -> bool {
    std::env::var("MCP_USE_INDEX_DB_TREE_SYNC")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(true)
}

fn sync_config() -> SyncConfig {
    *SYNC_CONFIG.get_or_init(|| {
        let max_retries =
            env_usize("MCP_COMMITMENT_TREE_SYNC_MAX_RETRIES", DEFAULT_SYNC_MAX_RETRIES).max(1);
        let retry_delay_ms = env_u64(
            "MCP_COMMITMENT_TREE_SYNC_RETRY_DELAY_MS",
            DEFAULT_SYNC_RETRY_DELAY_MS,
        );

        SyncConfig {
            max_retries,
            retry_delay: Duration::from_millis(retry_delay_ms),
        }
    })
}

fn position_lookup_config() -> PositionLookupConfig {
    *POSITION_LOOKUP_CONFIG.get_or_init(|| {
        let max_retries = env_usize(
            "MCP_COMMITMENT_TREE_POSITION_LOOKUP_MAX_RETRIES",
            DEFAULT_POSITION_LOOKUP_MAX_RETRIES,
        );
        let retry_delay_ms = env_u64(
            "MCP_COMMITMENT_TREE_POSITION_LOOKUP_RETRY_DELAY_MS",
            DEFAULT_POSITION_LOOKUP_RETRY_DELAY_MS,
        );

        PositionLookupConfig {
            max_retries,
            retry_delay: Duration::from_millis(retry_delay_ms),
        }
    })
}

fn required_depth_for_next_position(next_position: u64) -> Result<u8> {
    if next_position <= 1 {
        return Ok(0);
    }

    let depth_u32 = 64u32 - (next_position - 1).leading_zeros();
    let depth = u8::try_from(depth_u32).expect("depth_u32 <= 64");
    anyhow::ensure!(
        depth <= MAX_TREE_DEPTH,
        "Commitment tree requires depth {} to fit next_position {}, but MAX_TREE_DEPTH is {}",
        depth,
        next_position,
        MAX_TREE_DEPTH
    );
    Ok(depth)
}

fn sort_rollup_height_commitments(notes: &mut [IndexerNoteCreated]) {
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

fn reorder_snapshot_notes_for_db_sync(
    mut notes: Vec<IndexerNoteCreated>,
) -> Result<(Vec<IndexerNoteCreated>, &'static str)> {
    let first_with_height = notes.iter().position(|n| n.rollup_height.is_some());
    let Some(first_with_height) = first_with_height else {
        return Ok((notes, "legacy append order (no rollup_height metadata)"));
    };

    let has_any_without_height = notes.iter().any(|n| n.rollup_height.is_none());
    if !has_any_without_height {
        sort_rollup_height_commitments(&mut notes);
        return Ok((notes, "deterministic rollup_height sort"));
    }

    if notes[first_with_height..]
        .iter()
        .any(|n| n.rollup_height.is_none())
    {
        anyhow::bail!(
            "Indexer snapshot has interleaved NoteCreated rows with and without rollup_height; cannot deterministically order mixed history",
        );
    }

    sort_rollup_height_commitments(&mut notes[first_with_height..]);
    Ok((
        notes,
        "hybrid legacy-prefix + deterministic rollup_height suffix sort",
    ))
}

/// Return the process-wide commitment tree syncer (shared across all wallets/sessions).
pub fn global_tree_syncer() -> &'static CommitmentTreeSyncer {
    GLOBAL_TREE_SYNCER.get_or_init(|| CommitmentTreeSyncer::new(DEFAULT_TREE_DEPTH))
}

/// Start a single background task which keeps the global tree cache warm.
///
/// Safe to call multiple times: only the first call will spawn the task.
pub fn start_background_tree_sync(provider: Arc<Provider>) {
    BACKGROUND_SYNC_STARTED.get_or_init(|| {
        let syncer = global_tree_syncer();

        tokio::spawn(async move {
            // Don't spin on persistent errors (network, node restart, etc.).
            let mut last_error_at: Option<tokio::time::Instant> = None;

            loop {
                if let Err(e) = syncer.sync_to_latest(provider.as_ref()).await {
                    let now = tokio::time::Instant::now();
                    let should_log = last_error_at
                        .map(|t| now.duration_since(t) >= Duration::from_secs(10))
                        .unwrap_or(true);
                    if should_log {
                        tracing::warn!(error = %e, "Background commitment-tree sync failed");
                        last_error_at = Some(now);
                    }
                    tokio::time::sleep(Duration::from_secs(BACKGROUND_SYNC_ERROR_BACKOFF_SECS))
                        .await;
                    continue;
                }

                last_error_at = None;
                tokio::time::sleep(Duration::from_secs(BACKGROUND_SYNC_INTERVAL_SECS)).await;
            }
        });
    });
}

struct RootHex<'a>(&'a Hash32);

impl<'a> std::fmt::Display for RootHex<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct IncrementalSyncStats {
    start_offset: u64,
    target_next_position: u64,
    fetched_notes: usize,
    fetch_ms: u128,
    apply_ms: u128,
    rebuilt_root: Hash32,
    root_match: bool,
}

#[derive(Debug, Clone, Copy)]
struct FullRebuildStats {
    target_next_position: u64,
    fetched_notes: usize,
    tree_init_ms: u128,
    fetch_ms: u128,
    apply_ms: u128,
}

#[derive(Clone, Debug, Deserialize)]
struct TreeStateResp {
    #[serde(default)]
    depth: Option<u8>,
}

#[derive(Clone, Debug, Deserialize)]
struct NoteInfoResp {
    position: u64,
    commitment: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize)]
struct NotesResp {
    notes: Vec<NoteInfoResp>,
    #[serde(default)]
    current_root: Option<Vec<u8>>,
    #[serde(default)]
    count: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NotesSnapshot {
    root: Hash32,
    next_position: u64,
}

#[derive(Debug)]
struct NotesFetch {
    snapshot: NotesSnapshot,
    notes: Vec<(u64, Hash32)>,
}

#[derive(Debug)]
struct CachedTree {
    tree: MerkleTree,
    /// Number of filled leaves (next insertion position), as reported by the rollup.
    next_position: u64,
    /// Highest indexer `events.id` incorporated into this cache when DB sync is enabled.
    indexer_last_event_id: i64,
    /// Commitment -> position map for all leaves currently in the cache.
    pos_by_cm: HashMap<Hash32, u64>,
}

impl CachedTree {
    fn new(depth: u8) -> Self {
        Self {
            tree: MerkleTree::new(depth),
            next_position: 0,
            indexer_last_event_id: 0,
            pos_by_cm: HashMap::new(),
        }
    }
}

/// Shared, incremental syncer for the Midnight Privacy commitment tree.
///
/// Goals:
/// - Avoid rebuilding the full Merkle tree on each transfer (amortize to once per process).
/// - Incrementally append new leaves as the chain advances.
/// - Provide fast `cm -> position` lookups and Merkle openings.
#[derive(Debug)]
pub struct CommitmentTreeSyncer {
    default_depth: u8,
    state: RwLock<CachedTree>,
    sync_lock: Mutex<()>,
}

impl CommitmentTreeSyncer {
    pub fn new(default_depth: u8) -> Self {
        Self {
            default_depth,
            state: RwLock::new(CachedTree::new(default_depth)),
            sync_lock: Mutex::new(()),
        }
    }

    pub async fn reset_cache(&self) {
        let mut st = self.state.write().await;
        *st = CachedTree::new(self.default_depth);
    }

    /// Try to resolve all commitment positions and Merkle openings from the current in-memory
    /// cache without triggering any network sync.
    async fn try_resolve_from_cache(
        &self,
        cms: &[Hash32],
    ) -> Option<(Hash32, Vec<u64>, Vec<Vec<Hash32>>, u64, u128)> {
        let st = self.state.read().await;
        let mut positions = Vec::with_capacity(cms.len());
        for cm in cms {
            match st.pos_by_cm.get(cm).copied() {
                Some(pos) => positions.push(pos),
                None => return None,
            }
        }

        let open_start = Instant::now();
        let siblings = positions
            .iter()
            .map(|pos| st.tree.open(*pos as usize))
            .collect();
        let open_ms = open_start.elapsed().as_millis();

        Some((st.tree.root(), positions, siblings, st.next_position, open_ms))
    }

    /// Sync the local cached tree to the latest stable `/notes` snapshot.
    ///
    /// This is safe to call concurrently: only one task will perform network fetches and
    /// tree updates at a time.
    pub async fn sync_to_latest(&self, provider: &Provider) -> Result<()> {
        if use_index_db_tree_sync() && provider.has_index_db() {
            return self.sync_to_latest_from_index_db(provider).await;
        }

        let sync = sync_config();
        for attempt in 0..sync.max_retries {
            let started = Instant::now();

            // We use /tree/state only as a depth hint. Root/count come from /notes snapshots.
            let state_started = Instant::now();
            let state: TreeStateResp = match provider
                .query_rest_endpoint("/modules/midnight-privacy/tree/state")
                .await
                .context("Failed to query midnight-privacy tree state")
            {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        error = %e,
                        error_chain = %format!("{:#}", e),
                        "Commitment tree state query failed; retrying"
                    );
                    tokio::time::sleep(sync.retry_delay).await;
                    continue;
                }
            };
            let state_ms = state_started.elapsed().as_millis();
            if let Some(chain_depth) = state.depth {
                anyhow::ensure!(
                    chain_depth <= MAX_TREE_DEPTH,
                    "Unexpected commitment tree depth {} exceeds MAX_TREE_DEPTH {}",
                    chain_depth,
                    MAX_TREE_DEPTH
                );
            }

            let start_offset = { self.state.read().await.next_position };
            let fetch_started = Instant::now();
            let fetched = match fetch_notes(provider, start_offset as usize, None, None).await {
                Ok(fetched) => fetched,
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        start_offset,
                        error = %e,
                        error_chain = %format!("{:#}", e),
                        "Commitment tree notes snapshot fetch failed; retrying"
                    );
                    tokio::time::sleep(sync.retry_delay).await;
                    continue;
                }
            };
            let fetch_ms = fetch_started.elapsed().as_millis();
            let NotesFetch {
                snapshot,
                notes: incremental_notes,
            } = fetched;

            let expected_root = snapshot.root;
            let expected_next = snapshot.next_position;
            let required_depth = required_depth_for_next_position(expected_next)?;
            let expected_depth = match state.depth {
                Some(chain_depth) if chain_depth >= required_depth => {
                    chain_depth.max(self.default_depth)
                }
                Some(chain_depth) => {
                    tracing::warn!(
                        chain_depth,
                        required_depth,
                        "Commitment tree depth hint is lower than required by notes count; using required depth"
                    );
                    required_depth.max(self.default_depth)
                }
                None => required_depth.max(self.default_depth),
            };

            let capacity = 1u64.checked_shl(expected_depth as u32).ok_or_else(|| {
                anyhow::anyhow!("Invalid commitment tree depth: {}", expected_depth)
            })?;
            anyhow::ensure!(
                expected_next <= capacity,
                "Commitment tree next_position {} exceeds capacity {} for depth {}",
                expected_next,
                capacity,
                expected_depth
            );

            // Quick no-op check before acquiring the sync lock.
            {
                let st = self.state.read().await;
                if st.next_position == expected_next && st.tree.root() == expected_root {
                    tracing::debug!(
                        state_ms,
                        fetch_ms,
                        next_position = expected_next,
                        elapsed_ms = started.elapsed().as_millis(),
                        "Commitment tree cache already up-to-date"
                    );
                    return Ok(());
                }
            }

            // Only one task should perform the expensive sync at a time.
            let _guard = self.sync_lock.lock().await;

            // Another task may have synced while we waited for the lock.
            {
                let st = self.state.read().await;
                if st.next_position == expected_next && st.tree.root() == expected_root {
                    tracing::debug!(
                        state_ms,
                        fetch_ms,
                        next_position = expected_next,
                        elapsed_ms = started.elapsed().as_millis(),
                        "Commitment tree cache already up-to-date"
                    );
                    return Ok(());
                }
            }

            // Ensure cache isn't ahead of the chain (reorg / reset).
            let current_next = { self.state.read().await.next_position };
            if expected_next < current_next {
                tracing::warn!(
                    cached_next_position = current_next,
                    notes_next_position = expected_next,
                    "Commitment tree appears to have rewound; resetting local cache"
                );
                let mut st = self.state.write().await;
                *st = CachedTree::new(expected_depth);
            }

            // If the chain depth changed but next_position doesn't force a growth, reset the local
            // cache to match the chain depth so roots/openings are computed consistently.
            {
                let st = self.state.read().await;
                if st.tree.depth() != expected_depth && expected_next <= st.tree.len() as u64 {
                    tracing::warn!(
                        cached_depth = st.tree.depth(),
                        chain_depth = expected_depth,
                        "Commitment tree depth mismatch; resetting local cache"
                    );
                    drop(st);
                    let mut st = self.state.write().await;
                    *st = CachedTree::new(expected_depth);
                }
            }

            // Heuristic: On a cold cache, prefer a full rebuild. Also prefer a full rebuild if
            // the delta is so large that per-leaf `set_leaf()` updates would likely dominate.
            let cached_next = { self.state.read().await.next_position };
            let delta = expected_next.saturating_sub(cached_next);
            let depth_for_threshold = u64::from(expected_depth).max(1);
            let full_rebuild_threshold = capacity / depth_for_threshold;
            let should_skip_incremental = delta > 0 && delta >= full_rebuild_threshold;

            // Try incremental sync first; on mismatch/error, fall back to a full rebuild.
            if !should_skip_incremental {
                match self
                    .try_incremental_sync(
                        start_offset,
                        expected_next,
                        expected_root,
                        incremental_notes,
                        fetch_ms,
                    )
                    .await
                {
                    Ok(Some(stats)) if stats.root_match => {
                        let elapsed_ms = started.elapsed().as_millis();
                        tracing::debug!(
                            elapsed_ms,
                            state_ms,
                            fetch_ms = stats.fetch_ms,
                            apply_ms = stats.apply_ms,
                            fetched_notes = stats.fetched_notes,
                            start_offset = stats.start_offset,
                            target_next_position = stats.target_next_position,
                            "Commitment tree synced (incremental)"
                        );
                        return Ok(());
                    }
                    Ok(Some(stats)) => {
                        let elapsed_ms = started.elapsed().as_millis();
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            elapsed_ms,
                            state_ms,
                            start_offset = stats.start_offset,
                            target_next_position = stats.target_next_position,
                            fetched_notes = stats.fetched_notes,
                            fetch_ms = stats.fetch_ms,
                            apply_ms = stats.apply_ms,
                            rebuilt_root = %RootHex(&stats.rebuilt_root),
                            expected_root = %RootHex(&expected_root),
                            "Commitment tree incremental sync root mismatch; falling back to full rebuild"
                        );

                        // Avoid serving an inconsistent tree while rebuilding.
                        let mut st = self.state.write().await;
                        *st = CachedTree::new(expected_depth);
                    }
                    Ok(None) => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            start_offset,
                            expected_next,
                            "Commitment tree cache changed during incremental sync; retrying"
                        );
                        tokio::time::sleep(sync.retry_delay).await;
                        continue;
                    }
                    Err(e) => {
                        let cached = self.state.read().await;
                        let cached_next_position = cached.next_position;
                        let cached_depth = cached.tree.depth();
                        let cached_root = cached.tree.root();
                        drop(cached);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            notes_next_position = expected_next,
                            chain_depth_hint = ?state.depth,
                            expected_depth,
                            expected_root = %RootHex(&expected_root),
                            cached_next_position,
                            cached_depth,
                            cached_root = %RootHex(&cached_root),
                            error = %e,
                            error_chain = %format!("{:#}", e),
                            "Commitment tree incremental sync failed; falling back to full rebuild"
                        );
                    }
                }
            }

            match self
                .full_rebuild(provider, snapshot, expected_depth)
                .await
                .with_context(|| {
                    format!(
                        "Full rebuild failed (attempt {} of {})",
                        attempt + 1,
                        sync.max_retries
                    )
                }) {
                Ok(stats) => {
                    let elapsed_ms = started.elapsed().as_millis();
                    tracing::debug!(
                        elapsed_ms,
                        state_ms,
                        target_next_position = stats.target_next_position,
                        fetched_notes = stats.fetched_notes,
                        tree_init_ms = stats.tree_init_ms,
                        fetch_ms = stats.fetch_ms,
                        apply_ms = stats.apply_ms,
                        "Commitment tree synced (full rebuild)"
                    );
                    return Ok(());
                }
                Err(e) => {
                    let cached = self.state.read().await;
                    let cached_next_position = cached.next_position;
                    let cached_depth = cached.tree.depth();
                    let cached_root = cached.tree.root();
                    drop(cached);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        notes_next_position = expected_next,
                        chain_depth_hint = ?state.depth,
                        expected_depth,
                        expected_root = %RootHex(&expected_root),
                        cached_next_position,
                        cached_depth,
                        cached_root = %RootHex(&cached_root),
                        error = %e,
                        error_chain = %format!("{:#}", e),
                        "Commitment-tree full rebuild failed; retrying"
                    );
                    // Ensure we don't keep a partially-updated cache across retries.
                    let mut st = self.state.write().await;
                    *st = CachedTree::new(expected_depth);
                    drop(st);
                    tokio::time::sleep(sync.retry_delay).await;
                    continue;
                }
            }
        }

        let st = self.state.read().await;
        let cached_next_position = st.next_position;
        let cached_depth = st.tree.depth();
        let cached_root = st.tree.root();
        drop(st);
        anyhow::bail!(
            "Failed to sync commitment tree after {} attempts (cached_next_position={}, cached_depth={}, cached_root={})",
            sync.max_retries,
            cached_next_position,
            cached_depth,
            hex::encode(cached_root)
        );
    }

    async fn sync_to_latest_from_index_db(&self, provider: &Provider) -> Result<()> {
        let sync = sync_config();
        for attempt in 0..sync.max_retries {
            let started = Instant::now();
            // Serialize DB-backed snapshot reads + cache updates to avoid N parallel
            // callers racing on stale checkpoints and exhausting retry budget.
            let _guard = self.sync_lock.lock().await;
            let start_event_id = { self.state.read().await.indexer_last_event_id };

            let delta_fetch_started = Instant::now();
            let delta_batch = provider
                .fetch_midnight_note_created_since(start_event_id)
                .await
                .context("Failed to fetch commitment-tree snapshot from indexer DB")?
                .ok_or_else(|| {
                    anyhow::anyhow!("INDEX_DB-backed commitment-tree sync requested, but DB pool is not configured")
                })?;
            let delta_fetch_ms = delta_fetch_started.elapsed().as_millis();

            if delta_batch.upto_event_id < start_event_id {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = sync.max_retries,
                    cached_event_id = start_event_id,
                    db_event_id = delta_batch.upto_event_id,
                    "Indexer event stream appears to have rewound; resetting commitment-tree cache"
                );
                let mut st = self.state.write().await;
                *st = CachedTree::new(self.default_depth);
                drop(st);
                drop(_guard);
                tokio::time::sleep(sync.retry_delay).await;
                continue;
            }

            if delta_batch.upto_event_id == start_event_id && delta_batch.notes.is_empty() {
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis(),
                    delta_fetch_ms,
                    checkpoint = delta_batch.upto_event_id,
                    "Commitment tree cache already up-to-date (indexer DB)"
                );
                return Ok(());
            }

            if delta_batch.notes.is_empty() {
                let mut st = self.state.write().await;
                if delta_batch.upto_event_id < st.indexer_last_event_id {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        cached_event_id = st.indexer_last_event_id,
                        db_event_id = delta_batch.upto_event_id,
                        "Indexer event id regressed relative to cache; resetting commitment-tree cache"
                    );
                    *st = CachedTree::new(self.default_depth);
                    drop(st);
                    drop(_guard);
                    tokio::time::sleep(sync.retry_delay).await;
                    continue;
                }

                st.indexer_last_event_id = delta_batch.upto_event_id;
                let tree_size = st.next_position;
                drop(st);

                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis(),
                    delta_fetch_ms,
                    scanned_rows = delta_batch.rows_scanned,
                    checkpoint = delta_batch.upto_event_id,
                    tree_size,
                    "Commitment tree checkpoint advanced in indexer DB (no new notes)"
                );
                return Ok(());
            }

            let has_any_rollup_height = delta_batch.notes.iter().any(|n| n.rollup_height.is_some());
            if has_any_rollup_height {
                let snapshot_fetch_started = Instant::now();
                let snapshot = provider
                    .fetch_midnight_note_created_since(-1)
                    .await
                    .context(
                        "Failed to fetch full commitment-tree note snapshot from indexer DB",
                    )?
                    .ok_or_else(|| {
                        anyhow::anyhow!("INDEX_DB-backed commitment-tree sync requested, but DB pool is not configured")
                    })?;
                let snapshot_fetch_ms = snapshot_fetch_started.elapsed().as_millis();

                match reorder_snapshot_notes_for_db_sync(snapshot.notes) {
                    Ok((ordered_notes, ordering_mode)) => {
                        let apply_started = Instant::now();

                        let expected_next = ordered_notes.len() as u64;
                        let depth = required_depth_for_next_position(expected_next)?
                            .max(self.default_depth);
                        let mut tree = MerkleTree::new(depth);
                        if expected_next > tree.len() as u64 {
                            tree.grow_to_fit(expected_next as usize);
                        }

                        let mut pos_by_cm: HashMap<Hash32, u64> =
                            HashMap::with_capacity(ordered_notes.len());
                        for (pos, note) in ordered_notes.into_iter().enumerate() {
                            tree.set_leaf(pos, note.commitment);
                            pos_by_cm.insert(note.commitment, pos as u64);
                        }

                        let apply_ms = apply_started.elapsed().as_millis();
                        let mut st = self.state.write().await;
                        st.tree = tree;
                        st.pos_by_cm = pos_by_cm;
                        st.next_position = expected_next;
                        st.indexer_last_event_id = snapshot.upto_event_id;
                        let tree_size = st.next_position;
                        drop(st);

                        tracing::debug!(
                            elapsed_ms = started.elapsed().as_millis(),
                            delta_fetch_ms,
                            snapshot_fetch_ms,
                            apply_ms,
                            ordering_mode,
                            new_notes = delta_batch.notes.len(),
                            scanned_rows = delta_batch.rows_scanned,
                            snapshot_rows = snapshot.rows_scanned,
                            checkpoint = snapshot.upto_event_id,
                            tree_size,
                            "Commitment tree synced from indexer DB (snapshot rebuild)"
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            checkpoint = snapshot.upto_event_id,
                            error = %e,
                            error_chain = %format!("{:#}", e),
                            "Indexer DB snapshot could not be deterministically ordered; falling back to legacy append order"
                        );
                    }
                };

                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = sync.max_retries,
                    checkpoint = delta_batch.upto_event_id,
                    "Indexer DB snapshot contains legacy NoteCreated rows without usable rollup_height ordering; falling back to legacy append order"
                );
            }

            let apply_started = Instant::now();
            let mut st = self.state.write().await;

            if delta_batch.upto_event_id < st.indexer_last_event_id {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = sync.max_retries,
                    cached_event_id = st.indexer_last_event_id,
                    db_event_id = delta_batch.upto_event_id,
                    "Indexer event id regressed relative to cache; resetting commitment-tree cache"
                );
                *st = CachedTree::new(self.default_depth);
                drop(st);
                drop(_guard);
                tokio::time::sleep(sync.retry_delay).await;
                continue;
            }

            let new_note_count = delta_batch.notes.len();
            for note in delta_batch.notes {
                let cm = note.commitment;
                let pos = st.next_position;
                if pos as usize >= st.tree.len() {
                    st.tree.grow_to_fit(pos as usize + 1);
                }
                st.tree.set_leaf(pos as usize, cm);
                st.pos_by_cm.insert(cm, pos);
                st.next_position = pos + 1;
            }
            st.indexer_last_event_id = delta_batch.upto_event_id;
            let tree_size = st.next_position;
            let apply_ms = apply_started.elapsed().as_millis();
            drop(st);

            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis(),
                delta_fetch_ms,
                apply_ms,
                new_notes = new_note_count,
                scanned_rows = delta_batch.rows_scanned,
                checkpoint = delta_batch.upto_event_id,
                tree_size,
                "Commitment tree synced from indexer DB (legacy append order)"
            );
            return Ok(());
        }

        let st = self.state.read().await;
        anyhow::bail!(
            "Failed to sync commitment tree from indexer DB after {} attempts (cached_next_position={}, cached_depth={}, cached_root={}, checkpoint={})",
            sync.max_retries,
            st.next_position,
            st.tree.depth(),
            hex::encode(st.tree.root()),
            st.indexer_last_event_id
        );
    }

    /// Resolve positions and Merkle openings for all `cms`, using the cached tree.
    ///
    /// This method first tries to resolve directly from the in-memory cache. On miss, it performs
    /// one explicit `sync_to_latest()` and then retries cache lookups briefly while waiting for
    /// eventual visibility.
    pub async fn resolve_positions_and_openings(
        &self,
        provider: &Provider,
        cms: &[Hash32],
    ) -> Result<(Hash32, Vec<u64>, Vec<Vec<Hash32>>)> {
        anyhow::ensure!(!cms.is_empty(), "cms must not be empty");

        let lookup = position_lookup_config();
        let started = Instant::now();
        let mut sync_time_ms: u128 = 0;
        // Fast path: try serving directly from the in-memory cache.
        if let Some((root, positions, siblings, tree_size, open_ms)) =
            self.try_resolve_from_cache(cms).await
        {
            tracing::info!(
                attempt = 0,
                total_ms = started.elapsed().as_millis(),
                sync_ms = sync_time_ms,
                open_ms,
                cms = cms.len(),
                tree_size,
                "[TREE_TIMING] Resolved commitment positions"
            );
            return Ok((root, positions, siblings));
        }

        // Cache miss: perform one explicit sync, then poll cache for eventual visibility.
        let sync_start = Instant::now();
        self.sync_to_latest(provider).await?;
        sync_time_ms += sync_start.elapsed().as_millis();

        let post_sync_attempts = lookup.max_retries.saturating_add(1);
        for attempt in 1..=post_sync_attempts {
            if let Some((root, positions, siblings, tree_size, open_ms)) =
                self.try_resolve_from_cache(cms).await
            {
                tracing::info!(
                    attempt,
                    total_ms = started.elapsed().as_millis(),
                    sync_ms = sync_time_ms,
                    open_ms,
                    cms = cms.len(),
                    tree_size,
                    "[TREE_TIMING] Resolved commitment positions"
                );
                return Ok((root, positions, siblings));
            }

            if attempt == post_sync_attempts {
                break;
            }

            tokio::time::sleep(lookup.retry_delay).await;
        }

        let st = self.state.read().await;
        let missing: Vec<Hash32> = cms
            .iter()
            .copied()
            .filter(|cm| !st.pos_by_cm.contains_key(cm))
            .collect();
        let sample: Vec<String> = missing
            .iter()
            .take(5)
            .map(|cm| format!("{}", RootHex(cm)))
            .collect();
        anyhow::bail!(
            "Failed to resolve commitment positions in local tree cache (missing {} of {}): {}",
            missing.len(),
            cms.len(),
            sample.join(", ")
        );
    }

    /// Single-pass membership lookup for commitments in the local tree cache.
    ///
    /// This method checks the in-memory cache first and only performs one `sync_to_latest()` when
    /// at least one commitment is missing. Unlike `resolve_positions_and_openings`, it does not
    /// perform additional retry loops.
    pub async fn commitment_presence(
        &self,
        provider: &Provider,
        cms: &[Hash32],
    ) -> Result<Vec<bool>> {
        if cms.is_empty() {
            return Ok(Vec::new());
        }

        let mut presence = {
            let st = self.state.read().await;
            cms.iter()
                .map(|cm| st.pos_by_cm.contains_key(cm))
                .collect::<Vec<bool>>()
        };

        if presence.iter().all(|present| *present) {
            return Ok(presence);
        }

        self.sync_to_latest(provider).await?;
        presence = {
            let st = self.state.read().await;
            cms.iter()
                .map(|cm| st.pos_by_cm.contains_key(cm))
                .collect::<Vec<bool>>()
        };

        Ok(presence)
    }

    async fn try_incremental_sync(
        &self,
        start_offset: u64,
        expected_next: u64,
        expected_root: Hash32,
        notes: Vec<(u64, Hash32)>,
        fetch_ms: u128,
    ) -> Result<Option<IncrementalSyncStats>> {
        let apply_started = Instant::now();
        let mut st = self.state.write().await;
        // Another task can't have advanced this cache because we hold `sync_lock`, but keep
        // this check to avoid applying on a stale snapshot if we ever refactor locking.
        if st.next_position != start_offset {
            return Ok(None);
        }

        if expected_next as usize > st.tree.len() {
            st.tree.grow_to_fit(expected_next as usize);
        }

        let fetched_notes = notes.len();
        for (pos, cm) in notes {
            if pos as usize >= st.tree.len() {
                st.tree.grow_to_fit(pos as usize + 1);
            }
            st.tree.set_leaf(pos as usize, cm);
            st.pos_by_cm.insert(cm, pos);
        }
        st.next_position = expected_next;
        let apply_ms = apply_started.elapsed().as_millis();

        let rebuilt_root = st.tree.root();
        let root_match = rebuilt_root == expected_root;

        Ok(Some(IncrementalSyncStats {
            start_offset,
            target_next_position: expected_next,
            fetched_notes,
            fetch_ms,
            apply_ms,
            rebuilt_root,
            root_match,
        }))
    }

    async fn full_rebuild(
        &self,
        provider: &Provider,
        snapshot: NotesSnapshot,
        depth: u8,
    ) -> Result<FullRebuildStats> {
        let expected_next = snapshot.next_position;
        let expected_root = snapshot.root;
        let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();

        let mut fetched_notes = 0usize;
        if expected_next > 0 {
            let fetch_started = Instant::now();
            let notes = fetch_notes(provider, 0, Some(expected_next), Some(snapshot))
                .await?
                .notes;
            let fetch_ms = fetch_started.elapsed().as_millis();

            fetched_notes = notes.len();
            let apply_started = Instant::now();
            let mut leaves: Vec<Hash32> = vec![[0u8; 32]; expected_next as usize];
            for (pos, cm) in notes {
                if pos >= expected_next {
                    continue;
                }
                leaves[pos as usize] = cm;
                pos_by_cm.insert(cm, pos);
            }
            let apply_ms = apply_started.elapsed().as_millis();

            let tree_init_started = Instant::now();
            let tree = MerkleTree::from_filled_leaves(depth, &leaves);
            let tree_init_ms = tree_init_started.elapsed().as_millis();

            let rebuilt_root = tree.root();
            if rebuilt_root != expected_root {
                tracing::warn!(
                    target_next_position = expected_next,
                    depth,
                    fetched_notes,
                    rebuilt_root = %RootHex(&rebuilt_root),
                    expected_root = %RootHex(&expected_root),
                    "Commitment tree full rebuild root mismatch"
                );
            }
            anyhow::ensure!(
                rebuilt_root == expected_root,
                "Rebuilt tree root mismatch: rebuilt={} expected={}",
                hex::encode(rebuilt_root),
                hex::encode(expected_root)
            );

            let mut st = self.state.write().await;
            st.tree = tree;
            st.pos_by_cm = pos_by_cm;
            st.next_position = expected_next;

            return Ok(FullRebuildStats {
                target_next_position: expected_next,
                fetched_notes,
                tree_init_ms,
                fetch_ms,
                apply_ms,
            });
        }

        let fetch_ms = 0u128;
        let tree_init_started = Instant::now();
        let tree = MerkleTree::new(depth);
        let tree_init_ms = tree_init_started.elapsed().as_millis();

        let apply_ms = 0u128;
        let rebuilt_root = tree.root();
        if rebuilt_root != expected_root {
            tracing::warn!(
                target_next_position = expected_next,
                depth,
                rebuilt_root = %RootHex(&rebuilt_root),
                expected_root = %RootHex(&expected_root),
                "Commitment tree full rebuild root mismatch on empty tree"
            );
        }
        anyhow::ensure!(
            rebuilt_root == expected_root,
            "Rebuilt tree root mismatch: rebuilt={} expected={}",
            hex::encode(rebuilt_root),
            hex::encode(expected_root)
        );

        let mut st = self.state.write().await;
        st.tree = tree;
        st.pos_by_cm = pos_by_cm;
        st.next_position = expected_next;

        Ok(FullRebuildStats {
            target_next_position: expected_next,
            fetched_notes,
            tree_init_ms,
            fetch_ms,
            apply_ms,
        })
    }
}

fn notes_snapshot_from_response(resp: &NotesResp, endpoint: &str) -> Result<NotesSnapshot> {
    let root_bytes = resp.current_root.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "Notes endpoint {} response missing current_root snapshot metadata",
            endpoint
        )
    })?;
    anyhow::ensure!(
        root_bytes.len() == 32,
        "Notes endpoint {} returned current_root with unexpected length {}",
        endpoint,
        root_bytes.len()
    );
    let mut root = [0u8; 32];
    root.copy_from_slice(root_bytes);

    let next_position = resp.count.ok_or_else(|| {
        anyhow::anyhow!(
            "Notes endpoint {} response missing count snapshot metadata",
            endpoint
        )
    })?;

    Ok(NotesSnapshot {
        root,
        next_position,
    })
}

async fn fetch_notes(
    provider: &Provider,
    start_offset: usize,
    target_leaves: Option<u64>,
    expected_snapshot: Option<NotesSnapshot>,
) -> Result<NotesFetch> {
    let mut out: Vec<(u64, Hash32)> = Vec::new();
    let mut offset = start_offset;
    let mut target_leaves = target_leaves.or(expected_snapshot.map(|s| s.next_position));
    let mut snapshot = expected_snapshot;

    loop {
        if let Some(target) = target_leaves {
            let target_usize = usize::try_from(target).with_context(|| {
                format!(
                    "Notes snapshot next_position {} does not fit in usize on this platform",
                    target
                )
            })?;
            if offset >= target_usize {
                break;
            }
        }

        let endpoint = format!(
            "/modules/midnight-privacy/notes?limit={}&offset={}",
            NOTES_PAGE_LIMIT, offset
        );
        let mut empty_retries = 0usize;
        let resp: NotesResp = loop {
            let resp: NotesResp = provider
                .query_rest_endpoint(&endpoint)
                .await
                .with_context(|| format!("Failed to query notes batch at offset {}", offset))?;

            let page_snapshot = notes_snapshot_from_response(&resp, &endpoint)?;

            if let Some(expected) = snapshot {
                anyhow::ensure!(
                    page_snapshot.next_position >= expected.next_position,
                    "Notes snapshot rewound while fetching pages at offset {}: expected_count={} got_count={}",
                    offset,
                    expected.next_position,
                    page_snapshot.next_position
                );
                if page_snapshot != expected {
                    tracing::debug!(
                        offset,
                        expected_root = %hex::encode(expected.root),
                        expected_count = expected.next_position,
                        page_root = %hex::encode(page_snapshot.root),
                        page_count = page_snapshot.next_position,
                        "Notes snapshot advanced during paged fetch; continuing with initial snapshot target"
                    );
                }
            } else {
                snapshot = Some(page_snapshot);
            }

            if target_leaves.is_none() {
                target_leaves = Some(page_snapshot.next_position);
            }
            let target = target_leaves.expect("target_leaves is set");
            let target_usize = usize::try_from(target).with_context(|| {
                format!(
                    "Notes snapshot next_position {} does not fit in usize on this platform",
                    target
                )
            })?;

            if !resp.notes.is_empty() || offset >= target_usize {
                break resp;
            }

            if empty_retries >= NOTES_EMPTY_PAGE_MAX_RETRIES {
                anyhow::bail!(
                    "Notes endpoint returned empty batch at offset {} after {} retries (target_leaves={})",
                    offset,
                    NOTES_EMPTY_PAGE_MAX_RETRIES,
                    target
                );
            }

            empty_retries += 1;
            tracing::warn!(
                endpoint,
                offset,
                target_leaves = target,
                empty_retries,
                max_empty_retries = NOTES_EMPTY_PAGE_MAX_RETRIES,
                retry_delay_ms = NOTES_EMPTY_PAGE_RETRY_DELAY_MS,
                "Commitment tree notes endpoint returned empty page; retrying"
            );
            tokio::time::sleep(std::time::Duration::from_millis(
                NOTES_EMPTY_PAGE_RETRY_DELAY_MS,
            ))
            .await;
        };

        let batch_len = resp.notes.len();
        let target = target_leaves.expect("target_leaves is set");
        for n in resp.notes {
            if n.position >= target {
                continue;
            }
            if n.commitment.len() != 32 {
                anyhow::bail!(
                    "Note commitment has unexpected length {} at position {}",
                    n.commitment.len(),
                    n.position
                );
            }
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            out.push((n.position, cm));
        }

        if batch_len == 0 {
            break;
        }
        offset += batch_len;
    }

    let snapshot = snapshot
        .ok_or_else(|| anyhow::anyhow!("Failed to read notes snapshot metadata from /notes"))?;
    let target = target_leaves.unwrap_or(snapshot.next_position);
    let expected_count_u64 = target.saturating_sub(start_offset as u64);
    let expected_count = usize::try_from(expected_count_u64).with_context(|| {
        format!(
            "Expected note count {} does not fit in usize on this platform",
            expected_count_u64
        )
    })?;
    anyhow::ensure!(
        out.len() == expected_count,
        "Fetched {} notes but expected {} (start_offset={}, target_leaves={})",
        out.len(),
        expected_count,
        start_offset,
        target
    );
    Ok(NotesFetch {
        snapshot,
        notes: out,
    })
}
