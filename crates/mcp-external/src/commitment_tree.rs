use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use futures::FutureExt;
use midnight_privacy::{Hash32, MerkleTree, MAX_TREE_DEPTH};
use serde::Deserialize;
use tokio::sync::{watch, Mutex, RwLock};

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

// NOTE: use_index_db_tree_sync() removed — hybrid approach no longer needs
// a global toggle; Index DB is always tried first for rebuilds when available,
// with RPC as the canonical fallback for incremental syncs.

fn sync_config() -> SyncConfig {
    *SYNC_CONFIG.get_or_init(|| {
        let max_retries = env_usize(
            "MCP_COMMITMENT_TREE_SYNC_MAX_RETRIES",
            DEFAULT_SYNC_MAX_RETRIES,
        )
        .max(1);
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

#[derive(Debug)]
struct IndexDbRebuildResult {
    tree: Arc<MerkleTree>,
    pos_by_cm: HashMap<Hash32, u64>,
    next_position: u64,
    indexer_last_event_id: i64,
    root: Hash32,
    notes_count: usize,
    fetch_ms: u128,
    apply_ms: u128,
}

#[derive(Clone, Debug, Deserialize)]
struct TreeStateResp {
    // root and next_position are deserialized but not read directly;
    // we only use `depth` as a hint. Prefix with _ to suppress dead-code lint
    // while keeping the fields available for debugging / future use.
    #[serde(default)]
    _root: Option<Vec<u8>>,
    #[serde(default)]
    _next_position: Option<u64>,
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

#[derive(Clone, Debug, Deserialize)]
struct LedgerSlotResp {
    number: u64,
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
    /// The Merkle tree wrapped in Arc so that readers (opening computation) can
    /// clone the Arc and work without holding the RwLock, while writers use
    /// `Arc::make_mut` for copy-on-write semantics.
    tree: Arc<MerkleTree>,
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
            tree: Arc::new(MerkleTree::new(depth)),
            next_position: 0,
            indexer_last_event_id: 0,
            pos_by_cm: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum SyncFlightOutcome {
    Success,
    Error(String),
}

#[derive(Debug)]
struct SyncFlightState {
    in_flight: bool,
    completed_round: u64,
    outcome: SyncFlightOutcome,
}

impl Default for SyncFlightState {
    fn default() -> Self {
        Self {
            in_flight: false,
            completed_round: 0,
            outcome: SyncFlightOutcome::Success,
        }
    }
}

fn sync_flight_outcome_to_result(outcome: &SyncFlightOutcome) -> Result<()> {
    match outcome {
        SyncFlightOutcome::Success => Ok(()),
        SyncFlightOutcome::Error(error) => anyhow::bail!(error.clone()),
    }
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
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
    sync_flight: Mutex<SyncFlightState>,
    sync_flight_tx: watch::Sender<u64>,
}

impl CommitmentTreeSyncer {
    pub fn new(default_depth: u8) -> Self {
        let (sync_flight_tx, _sync_flight_rx) = watch::channel(0);
        Self {
            default_depth,
            state: RwLock::new(CachedTree::new(default_depth)),
            sync_lock: Mutex::new(()),
            sync_flight: Mutex::new(SyncFlightState::default()),
            sync_flight_tx,
        }
    }

    pub async fn reset_cache(&self) {
        let mut st = self.state.write().await;
        *st = CachedTree::new(self.default_depth);
    }

    /// Return the latest published sync round for this process-wide tree cache.
    pub fn current_sync_round(&self) -> u64 {
        *self.sync_flight_tx.borrow()
    }

    /// Wait for a new sync round to be published, or timeout.
    ///
    /// Returns `true` when a newer round was observed, `false` on timeout.
    pub async fn wait_for_sync_round_advance(
        &self,
        observed_round: u64,
        timeout: Duration,
    ) -> bool {
        let mut rx = self.sync_flight_tx.subscribe();
        if *rx.borrow() != observed_round {
            return true;
        }
        matches!(
            tokio::time::timeout(timeout, rx.changed()).await,
            Ok(Ok(()))
        )
    }

    /// Sync the local cached tree to the latest stable `/notes` snapshot.
    ///
    /// This is safe to call concurrently. When many tasks request a sync at once, a
    /// single leader performs the sync and followers reuse that outcome.
    pub async fn sync_to_latest(&self, provider: &Provider) -> Result<()> {
        loop {
            let mut rx = self.sync_flight_tx.subscribe();
            let observed_round = {
                let mut flight = self.sync_flight.lock().await;
                if !flight.in_flight {
                    flight.in_flight = true;
                    None
                } else {
                    Some(flight.completed_round)
                }
            };

            if let Some(round) = observed_round {
                // Another task is syncing. Wait for the round to advance and reuse its outcome.
                if *rx.borrow() == round {
                    let _ = rx.changed().await;
                }

                let flight = self.sync_flight.lock().await;
                if flight.completed_round != round {
                    return sync_flight_outcome_to_result(&flight.outcome);
                }
                continue;
            }

            let result = match std::panic::AssertUnwindSafe(self.sync_to_latest_impl(provider))
                .catch_unwind()
                .await
            {
                Ok(res) => res,
                Err(panic_payload) => Err(anyhow::anyhow!(
                    "Commitment tree sync panicked: {}",
                    panic_payload_to_string(panic_payload)
                )),
            };
            let outcome = match &result {
                Ok(()) => SyncFlightOutcome::Success,
                Err(err) => SyncFlightOutcome::Error(format!("{:#}", err)),
            };

            let new_round = {
                let mut flight = self.sync_flight.lock().await;
                flight.in_flight = false;
                flight.completed_round = flight.completed_round.wrapping_add(1);
                flight.outcome = outcome;
                flight.completed_round
            };
            let _ = self.sync_flight_tx.send(new_round);

            return result;
        }
    }

    async fn sync_to_latest_impl(&self, provider: &Provider) -> Result<()> {
        // ---------- Hybrid strategy ----------
        //
        // Index DB path  → fast bulk load (single SQL query), but ordering may diverge
        //                  from canonical on-chain order under concurrent load.
        // RPC/REST path  → authoritative canonical ordering + built-in root validation,
        //                  but slower for large rebuilds (paginated HTTP).
        //
        // Hybrid approach:
        //   • Full rebuilds (cold cache / large delta) → prefer Index DB for speed,
        //     validate result with `is_valid_anchor`, fall back to RPC if invalid.
        //   • Incremental syncs → always use RPC (correct by construction).

        let sync = sync_config();
        for attempt in 0..sync.max_retries {
            let started = Instant::now();

            let slot_number_started = Instant::now();
            let sync_slot_number = match fetch_latest_slot_number(provider).await {
                Ok(slot_number) => slot_number,
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        error = %e,
                        error_chain = %format!("{:#}", e),
                        "Failed to fetch latest slot number for commitment-tree sync; retrying"
                    );
                    tokio::time::sleep(sync.retry_delay).await;
                    continue;
                }
            };
            let slot_number_ms = slot_number_started.elapsed().as_millis();

            // We use /tree/state only as a depth hint. Root/count come from /notes snapshots.
            let state_started = Instant::now();
            let tree_state_endpoint =
                append_slot_number_query("/modules/midnight-privacy/tree/state", sync_slot_number);
            let state: TreeStateResp = match provider
                .query_rest_endpoint(&tree_state_endpoint)
                .await
                .context("Failed to query midnight-privacy tree state")
            {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        slot_number = sync_slot_number,
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
            let fetched = match fetch_notes(
                provider,
                start_offset as usize,
                None,
                None,
                sync_slot_number,
            )
            .await
            {
                Ok(fetched) => fetched,
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = sync.max_retries,
                        slot_number = sync_slot_number,
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
                        slot_number = sync_slot_number,
                        slot_number_ms,
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
                        slot_number = sync_slot_number,
                        slot_number_ms,
                        state_ms,
                        fetch_ms,
                        next_position = expected_next,
                        elapsed_ms = started.elapsed().as_millis(),
                        "Commitment tree cache already up-to-date"
                    );
                    return Ok(());
                }
            }

            // If this snapshot is behind our cache, treat it as stale and retry.
            // With single-flight enabled, this is typically endpoint lag, not a real rewind.
            let current_next = { self.state.read().await.next_position };
            if expected_next < current_next {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = sync.max_retries,
                    slot_number = sync_slot_number,
                    cached_next_position = current_next,
                    notes_next_position = expected_next,
                    "Commitment tree snapshot is behind cache; retrying without reset"
                );
                drop(_guard);
                tokio::time::sleep(sync.retry_delay).await;
                continue;
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

            // Try incremental sync first (RPC path — canonical ordering + root validation).
            if !should_skip_incremental {
                match self
                    .try_incremental_sync(
                        start_offset,
                        expected_next,
                        expected_root,
                        expected_depth,
                        incremental_notes,
                        fetch_ms,
                    )
                    .await
                {
                    Ok(Some(stats)) if stats.root_match => {
                        let elapsed_ms = started.elapsed().as_millis();
                        tracing::debug!(
                            slot_number = sync_slot_number,
                            slot_number_ms,
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
                            slot_number = sync_slot_number,
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
                    }
                    Ok(None) => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            slot_number = sync_slot_number,
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
                            slot_number = sync_slot_number,
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

            let is_cold_cache = cached_next == 0;
            let rebuild_trigger = if should_skip_incremental {
                if is_cold_cache {
                    "cold_cache"
                } else {
                    "large_delta"
                }
            } else {
                "incremental_fallback"
            };

            // --- Full rebuild ---
            // Only try Index DB on cold start (empty cache).  On warm-cache fallbacks
            // (incremental_fallback / large_delta), the DB consistently produces invalid
            // roots because its ordering lags behind the canonical on-chain order.
            // Going straight to the canonical RPC rebuild avoids the wasted DB round-trip
            // AND prevents a window where an invalid DB root could be observed by readers.
            if is_cold_cache && provider.has_index_db() {
                match self
                    .build_tree_from_index_db(provider, expected_depth)
                    .await
                {
                    Ok(db_result) => {
                        let db_root = db_result.root;
                        let db_next = db_result.next_position;
                        // Validate the DB-built root against the chain BEFORE
                        // committing to the cache.  This closes the window where
                        // concurrent readers could observe a fabricated root.
                        let anchor_valid = match provider.is_valid_anchor(&db_root).await {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    attempt = attempt + 1,
                                    error = %e,
                                    slot_number = sync_slot_number,
                                    "is_valid_anchor check failed after Index DB rebuild; \
                                             falling back to RPC (will not assume valid)"
                                );
                                false
                            }
                        };

                        if anchor_valid {
                            // Only NOW commit the validated tree to the cache.
                            let mut st = self.state.write().await;
                            st.tree = db_result.tree;
                            st.pos_by_cm = db_result.pos_by_cm;
                            st.next_position = db_result.next_position;
                            st.indexer_last_event_id = db_result.indexer_last_event_id;
                            drop(st);

                            let elapsed_ms = started.elapsed().as_millis();
                            tracing::info!(
                                elapsed_ms,
                                state_ms,
                                rebuild_trigger,
                                rebuild_source = "index_db",
                                depth = expected_depth,
                                db_fetch_ms = db_result.fetch_ms,
                                db_apply_ms = db_result.apply_ms,
                                db_notes = db_result.notes_count,
                                tree_size = db_next,
                                "Commitment tree synced (full rebuild from Index DB, anchor valid)"
                            );
                            return Ok(());
                        }

                        // DB root is not a valid anchor — ordering diverged.
                        // We did NOT write to cache, so no reset needed.
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            slot_number = sync_slot_number,
                            rebuild_trigger,
                            db_root = %RootHex(&db_root),
                            db_next_position = db_next,
                            "Index DB rebuild produced invalid anchor root; \
                             falling back to RPC full rebuild"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = sync.max_retries,
                            slot_number = sync_slot_number,
                            rebuild_trigger,
                            error = %e,
                            error_chain = %format!("{:#}", e),
                            "Index DB full rebuild failed; falling back to RPC full rebuild"
                        );
                    }
                }
            }

            // RPC full rebuild (last resort, always canonical).
            match self
                .full_rebuild(provider, snapshot, expected_depth, sync_slot_number)
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
                    let tree_mem_bytes: u128 = (capacity as u128)
                        .saturating_mul(2u128)
                        .saturating_mul(std::mem::size_of::<Hash32>() as u128);
                    tracing::debug!(
                        slot_number = sync_slot_number,
                        slot_number_ms,
                        elapsed_ms,
                        state_ms,
                        rebuild_trigger,
                        rebuild_source = "rpc",
                        depth = expected_depth,
                        capacity,
                        cached_next_before = cached_next,
                        delta,
                        full_rebuild_threshold,
                        tree_mem_bytes,
                        target_next_position = stats.target_next_position,
                        fetched_notes = stats.fetched_notes,
                        tree_init_ms = stats.tree_init_ms,
                        fetch_ms = stats.fetch_ms,
                        apply_ms = stats.apply_ms,
                        "Commitment tree synced (full rebuild from RPC)"
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
                        slot_number = sync_slot_number,
                        rebuild_trigger,
                        depth = expected_depth,
                        capacity,
                        delta,
                        full_rebuild_threshold,
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

    /// Try to resolve all commitment positions and Merkle openings from the current in-memory
    /// cache without triggering any network sync.
    ///
    /// The read lock is held only briefly to look up positions and clone the `Arc<MerkleTree>`.
    /// Opening computation (O(depth) per commitment) runs entirely without any lock, so
    /// concurrent writers are never blocked by opening computation.
    async fn try_resolve_from_cache(
        &self,
        cms: &[Hash32],
    ) -> Option<(Hash32, Vec<u64>, Vec<Vec<Hash32>>, u64, u128)> {
        // Phase 1: Look up positions and snapshot the tree Arc under a brief read lock.
        let (tree, positions, next_position) = {
            let st = self.state.read().await;
            let mut positions = Vec::with_capacity(cms.len());
            for cm in cms {
                match st.pos_by_cm.get(cm).copied() {
                    Some(pos) => positions.push(pos),
                    None => return None,
                }
            }
            (Arc::clone(&st.tree), positions, st.next_position)
        };
        // Read lock released — writers are no longer blocked.

        // Phase 2: Compute openings from the snapshot without holding any lock.
        let open_started = Instant::now();
        let root = tree.root();
        let siblings = positions
            .iter()
            .map(|pos| tree.open(*pos as usize))
            .collect();
        let open_ms = open_started.elapsed().as_millis();

        Some((root, positions, siblings, next_position, open_ms))
    }

    /// Resolve positions and Merkle openings for all `cms`, using the cached tree.
    ///
    /// This method first tries the in-memory cache. On miss, it performs one explicit sync and
    /// then retries cache lookups briefly for eventual visibility.
    pub async fn resolve_positions_and_openings(
        &self,
        provider: &Provider,
        cms: &[Hash32],
    ) -> Result<(Hash32, Vec<u64>, Vec<Vec<Hash32>>)> {
        anyhow::ensure!(!cms.is_empty(), "cms must not be empty");

        let lookup = position_lookup_config();
        let started = Instant::now();
        let mut sync_time_ms: u128 = 0;

        // Fast path: serve directly from cache.
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

        // Miss path: one explicit sync, then poll cache briefly.
        let sync_started = Instant::now();
        self.sync_to_latest(provider).await?;
        sync_time_ms += sync_started.elapsed().as_millis();
        let mut observed_round = self.current_sync_round();

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

            // Prefer waiting for published tree updates. If no update arrives before the retry
            // delay, trigger one explicit sync attempt and continue.
            if self
                .wait_for_sync_round_advance(observed_round, lookup.retry_delay)
                .await
            {
                observed_round = self.current_sync_round();
                continue;
            }

            let sync_started = Instant::now();
            self.sync_to_latest(provider).await?;
            sync_time_ms += sync_started.elapsed().as_millis();
            observed_round = self.current_sync_round();
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

    /// Cached-only membership lookup for commitments in the local tree cache.
    pub async fn commitment_presence_cached(&self, cms: &[Hash32]) -> Vec<bool> {
        if cms.is_empty() {
            return Vec::new();
        }

        let st = self.state.read().await;
        cms.iter().map(|cm| st.pos_by_cm.contains_key(cm)).collect()
    }

    /// Membership lookup for commitments in the local tree cache.
    ///
    /// This checks the in-memory cache first and only performs one explicit sync when at least
    /// one commitment is missing.
    pub async fn commitment_presence(
        &self,
        provider: &Provider,
        cms: &[Hash32],
    ) -> Result<Vec<bool>> {
        if cms.is_empty() {
            return Ok(Vec::new());
        }

        let cached = self.commitment_presence_cached(cms).await;
        if cached.iter().all(|present| *present) {
            return Ok(cached);
        }

        self.sync_to_latest(provider).await?;
        Ok(self.commitment_presence_cached(cms).await)
    }

    async fn try_incremental_sync(
        &self,
        start_offset: u64,
        expected_next: u64,
        expected_root: Hash32,
        expected_depth: u8,
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

        let fetched_notes = notes.len();
        st.pos_by_cm.reserve(fetched_notes);

        // Mutate tree through Arc::make_mut (copy-on-write if readers exist).
        // Pre-grow once to final size — no per-leaf capacity checks needed.
        let tree = Arc::make_mut(&mut st.tree);
        if expected_next as usize > tree.len() {
            tree.grow_to_fit(expected_next as usize);
        }
        let mut contiguous_cms = Vec::with_capacity(fetched_notes);
        let mut is_contiguous = true;
        for (i, (pos, cm)) in notes.iter().enumerate() {
            match start_offset.checked_add(i as u64) {
                Some(expected) if *pos == expected => contiguous_cms.push(*cm),
                _ => {
                    is_contiguous = false;
                    break;
                }
            }
        }
        if is_contiguous {
            let start = start_offset as usize;
            tree.set_leaves_contiguous(start, &contiguous_cms);
            for (i, cm) in contiguous_cms.into_iter().enumerate() {
                let pos = start_offset.checked_add(i as u64).ok_or_else(|| {
                    anyhow::anyhow!("Commitment position overflow while inserting contiguous notes")
                })?;
                st.pos_by_cm.insert(cm, pos);
            }
        } else {
            for &(pos, cm) in &notes {
                tree.set_leaf(pos as usize, cm);
            }
            for (pos, cm) in notes {
                st.pos_by_cm.insert(cm, pos);
            }
        }
        // tree borrow ends (NLL) — st is accessible again.
        st.next_position = expected_next;
        let apply_ms = apply_started.elapsed().as_millis();

        let rebuilt_root = st.tree.root();
        let root_match = rebuilt_root == expected_root;
        if !root_match {
            // Never expose a fabricated/inconsistent root to concurrent readers.
            // Reset while still holding the write lock; caller will rebuild from snapshot.
            *st = CachedTree::new(expected_depth);
        }

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

    /// Build a commitment tree from the Index DB **without writing to the cache**.
    ///
    /// Loads all notes from `midnight_note_created` in a single SQL query, sorts them
    /// by `(rollup_height, commitment)` for canonical ordering, and builds the tree.
    ///
    /// The caller MUST validate the resulting root with `is_valid_anchor` and only
    /// commit the tree to the cache after successful validation.  This prevents a
    /// window where concurrent readers could observe a fabricated (non-canonical) root.
    async fn build_tree_from_index_db(
        &self,
        provider: &Provider,
        depth: u8,
    ) -> Result<IndexDbRebuildResult> {
        let fetch_started = Instant::now();
        let batch = provider
            .fetch_midnight_note_created_since(-1)
            .await
            .context("Index DB full rebuild: failed to fetch all notes")?
            .ok_or_else(|| {
                anyhow::anyhow!("Index DB full rebuild requested but DB pool is not configured")
            })?;
        let fetch_ms = fetch_started.elapsed().as_millis();

        let upto_event_id = batch.upto_event_id;
        let notes_count = batch.notes.len();

        // Sort into canonical order: (rollup_height ASC, commitment ASC).
        let mut notes = batch.notes;
        let has_all_heights = notes.iter().all(|n| n.rollup_height.is_some());
        if has_all_heights && !notes.is_empty() {
            sort_rollup_height_commitments(&mut notes);
        } else if !notes.is_empty() {
            // Mixed or missing rollup_height — attempt reorder, bail if impossible.
            notes = reorder_snapshot_notes_for_db_sync(notes)
                .context("Index DB full rebuild: cannot determine canonical ordering")?
                .0;
        }

        let default_depth = depth.max(self.default_depth);
        let apply_started = Instant::now();
        let (tree, pos_by_cm, expected_next) = tokio::task::spawn_blocking(move || -> Result<_> {
            let expected_next = notes.len() as u64;
            let depth = required_depth_for_next_position(expected_next)?.max(default_depth);

            let mut leaves: Vec<Hash32> = Vec::with_capacity(notes.len());
            let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::with_capacity(notes.len());
            for (pos, note) in notes.into_iter().enumerate() {
                leaves.push(note.commitment);
                pos_by_cm.insert(note.commitment, pos as u64);
            }

            let tree = MerkleTree::from_filled_leaves(depth, &leaves);
            Ok((Arc::new(tree), pos_by_cm, expected_next))
        })
        .await
        .map_err(|e| {
            anyhow::anyhow!("Index DB full rebuild tree construction panicked: {}", e)
        })??;
        let apply_ms = apply_started.elapsed().as_millis();

        let root = tree.root();

        // NOTE: We intentionally do NOT write to the cache here.
        // The caller must validate the root first via is_valid_anchor,
        // then commit to the cache only if valid.  This prevents exposing
        // fabricated roots to concurrent readers.

        Ok(IndexDbRebuildResult {
            tree,
            pos_by_cm,
            next_position: expected_next,
            indexer_last_event_id: upto_event_id,
            root,
            notes_count,
            fetch_ms,
            apply_ms,
        })
    }

    async fn full_rebuild(
        &self,
        provider: &Provider,
        snapshot: NotesSnapshot,
        depth: u8,
        slot_number: u64,
    ) -> Result<FullRebuildStats> {
        let expected_next = snapshot.next_position;
        let expected_root = snapshot.root;
        let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();

        let mut fetched_notes = 0usize;
        if expected_next > 0 {
            let fetch_started = Instant::now();
            let notes = fetch_notes(
                provider,
                0,
                Some(expected_next),
                Some(snapshot),
                slot_number,
            )
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

            // Move heavy tree construction off the async runtime.
            let tree_init_started = Instant::now();
            let (tree, rebuilt_root) = tokio::task::spawn_blocking(move || {
                let tree = MerkleTree::from_filled_leaves(depth, &leaves);
                let root = tree.root();
                (tree, root)
            })
            .await
            .map_err(|e| {
                anyhow::anyhow!("Tree construction panicked during full rebuild: {}", e)
            })?;
            let tree_init_ms = tree_init_started.elapsed().as_millis();

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

            // Brief lock to swap in the pre-built tree.
            let mut st = self.state.write().await;
            st.tree = Arc::new(tree);
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
        st.tree = Arc::new(tree);
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

fn append_slot_number_query(endpoint: &str, slot_number: u64) -> String {
    if endpoint.contains('?') {
        format!("{endpoint}&slot_number={slot_number}")
    } else {
        format!("{endpoint}?slot_number={slot_number}")
    }
}

async fn fetch_latest_slot_number(provider: &Provider) -> Result<u64> {
    let latest_slot: LedgerSlotResp = provider
        .query_rest_endpoint("/ledger/slots/finalized")
        .await
        .context("Failed to query latest slot number from /ledger/slots/finalized")?;
    Ok(latest_slot.number)
}

async fn fetch_notes(
    provider: &Provider,
    start_offset: usize,
    target_leaves: Option<u64>,
    expected_snapshot: Option<NotesSnapshot>,
    slot_number: u64,
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
        let endpoint = append_slot_number_query(&endpoint, slot_number);
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
