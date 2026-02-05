use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use midnight_privacy::{Hash32, MerkleTree, MAX_TREE_DEPTH};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::provider::Provider;

/// Default commitment-tree depth used by Midnight Privacy.
pub const DEFAULT_TREE_DEPTH: u8 = 16;

const NOTES_PAGE_LIMIT: usize = 1000;
const NOTES_EMPTY_PAGE_MAX_RETRIES: usize = 5;
const NOTES_EMPTY_PAGE_RETRY_DELAY_MS: u64 = 100;
const SYNC_MAX_RETRIES: usize = 3;
const SYNC_RETRY_DELAY_MS: u64 = 200;

const DEFAULT_POSITION_LOOKUP_MAX_RETRIES: usize = 50;
const DEFAULT_POSITION_LOOKUP_RETRY_DELAY_MS: u64 = 200;

const BACKGROUND_SYNC_INTERVAL_SECS: u64 = 1;
const BACKGROUND_SYNC_ERROR_BACKOFF_SECS: u64 = 3;

static GLOBAL_TREE_SYNCER: OnceLock<CommitmentTreeSyncer> = OnceLock::new();
static BACKGROUND_SYNC_STARTED: OnceLock<()> = OnceLock::new();
static POSITION_LOOKUP_CONFIG: OnceLock<PositionLookupConfig> = OnceLock::new();

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
    root: Vec<u8>,
    next_position: u64,
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
    #[allow(dead_code)]
    current_root: Option<Vec<u8>>,
    #[serde(default)]
    #[allow(dead_code)]
    count: Option<u64>,
}

#[derive(Debug)]
struct CachedTree {
    tree: MerkleTree,
    /// Number of filled leaves (next insertion position), as reported by the rollup.
    next_position: u64,
    /// Commitment -> position map for all leaves currently in the cache.
    pos_by_cm: HashMap<Hash32, u64>,
}

impl CachedTree {
    fn new(depth: u8) -> Self {
        Self {
            tree: MerkleTree::new(depth),
            next_position: 0,
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

    /// Sync the local cached tree to (at least) the current `/tree/state`.
    ///
    /// This is safe to call concurrently: only one task will perform network fetches and
    /// tree updates at a time.
    pub async fn sync_to_latest(&self, provider: &Provider) -> Result<()> {
        for attempt in 0..SYNC_MAX_RETRIES {
            let started = Instant::now();
            let state_started = Instant::now();
            let state: TreeStateResp = provider
                .query_rest_endpoint("/modules/midnight-privacy/tree/state")
                .await
                .context("Failed to query midnight-privacy tree state")?;
            let state_ms = state_started.elapsed().as_millis();

            anyhow::ensure!(
                state.root.len() == 32,
                "Tree state root has unexpected length: {}",
                state.root.len()
            );

            let mut expected_root = [0u8; 32];
            expected_root.copy_from_slice(&state.root);
            let expected_next = state.next_position;
            let required_depth = required_depth_for_next_position(expected_next)?;
            let expected_depth = match state.depth {
                Some(chain_depth) => {
                    anyhow::ensure!(
                        chain_depth <= MAX_TREE_DEPTH,
                        "Unexpected commitment tree depth {} exceeds MAX_TREE_DEPTH {}",
                        chain_depth,
                        MAX_TREE_DEPTH
                    );
                    anyhow::ensure!(
                        chain_depth >= required_depth,
                        "Commitment tree depth {} too small for next_position {} (requires >= {})",
                        chain_depth,
                        expected_next,
                        required_depth
                    );
                    chain_depth
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
                    chain_next_position = expected_next,
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
                    .try_incremental_sync(provider, expected_next, expected_root)
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
                            max_attempts = SYNC_MAX_RETRIES,
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
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = SYNC_MAX_RETRIES,
                            error = %e,
                            "Commitment tree incremental sync failed; falling back to full rebuild"
                        );
                    }
                }
            }

            match self
                .full_rebuild(provider, expected_next, expected_root, expected_depth)
                .await
                .with_context(|| {
                    format!(
                        "Full rebuild failed (attempt {} of {})",
                        attempt + 1,
                        SYNC_MAX_RETRIES
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
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts = SYNC_MAX_RETRIES,
                        error = %e,
                        "Commitment-tree full rebuild failed; retrying"
                    );
                    // Ensure we don't keep a partially-updated cache across retries.
                    let mut st = self.state.write().await;
                    *st = CachedTree::new(expected_depth);
                    drop(st);
                    tokio::time::sleep(std::time::Duration::from_millis(SYNC_RETRY_DELAY_MS)).await;
                    continue;
                }
            }
        }

        anyhow::bail!(
            "Failed to sync commitment tree after {} attempts",
            SYNC_MAX_RETRIES
        );
    }

    /// Resolve positions and Merkle openings for all `cms`, using the cached tree.
    ///
    /// This method performs a `sync_to_latest()` first, and then retries position resolution
    /// briefly in case the caller's notes were just created and haven't been appended yet.
    pub async fn resolve_positions_and_openings(
        &self,
        provider: &Provider,
        cms: &[Hash32],
    ) -> Result<(Hash32, Vec<u64>, Vec<Vec<Hash32>>)> {
        anyhow::ensure!(!cms.is_empty(), "cms must not be empty");

        let lookup = position_lookup_config();
        let started = Instant::now();
        let mut sync_time_ms: u128 = 0;
        let open_time_ms: u128;

        for attempt in 0..=lookup.max_retries {
            // Sync first (cheap no-op if already up-to-date), then resolve positions/openings.
            let sync_start = Instant::now();
            self.sync_to_latest(provider).await?;
            sync_time_ms += sync_start.elapsed().as_millis();
            {
                let st = self.state.read().await;
                let mut positions = Vec::with_capacity(cms.len());
                let mut missing = false;
                for cm in cms {
                    match st.pos_by_cm.get(cm).copied() {
                        Some(pos) => positions.push(pos),
                        None => {
                            missing = true;
                            break;
                        }
                    }
                }

                if !missing {
                    let open_start = Instant::now();
                    let siblings = positions
                        .iter()
                        .map(|pos| st.tree.open(*pos as usize))
                        .collect();
                    open_time_ms = open_start.elapsed().as_millis();

                    tracing::info!(
                        attempt,
                        total_ms = started.elapsed().as_millis(),
                        sync_ms = sync_time_ms,
                        open_ms = open_time_ms,
                        cms = cms.len(),
                        tree_size = st.next_position,
                        "[TREE_TIMING] Resolved commitment positions"
                    );
                    return Ok((st.tree.root(), positions, siblings));
                }
            }

            if attempt == lookup.max_retries {
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

    async fn try_incremental_sync(
        &self,
        provider: &Provider,
        expected_next: u64,
        expected_root: Hash32,
    ) -> Result<Option<IncrementalSyncStats>> {
        let start_offset = { self.state.read().await.next_position };
        if start_offset >= expected_next {
            // Either up to date (handled earlier) or inconsistent; force full rebuild.
            return Ok(None);
        }

        let fetch_started = Instant::now();
        let notes = fetch_notes(provider, start_offset as usize, expected_next as usize).await?;
        let fetch_ms = fetch_started.elapsed().as_millis();

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
        expected_next: u64,
        expected_root: Hash32,
        depth: u8,
    ) -> Result<FullRebuildStats> {
        let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();

        let mut fetched_notes = 0usize;
        if expected_next > 0 {
            let fetch_started = Instant::now();
            let notes = fetch_notes(provider, 0, expected_next as usize).await?;
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

async fn fetch_notes(
    provider: &Provider,
    start_offset: usize,
    target_leaves: usize,
) -> Result<Vec<(u64, Hash32)>> {
    let expected_count = target_leaves.saturating_sub(start_offset);
    let mut out: Vec<(u64, Hash32)> = Vec::with_capacity(expected_count);
    let mut offset = start_offset;

    while offset < target_leaves {
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

            if !resp.notes.is_empty() {
                break resp;
            }

            if empty_retries >= NOTES_EMPTY_PAGE_MAX_RETRIES {
                anyhow::bail!(
                    "Notes endpoint returned empty batch at offset {} after {} retries (target_leaves={})",
                    offset,
                    NOTES_EMPTY_PAGE_MAX_RETRIES,
                    target_leaves
                );
            }

            empty_retries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(
                NOTES_EMPTY_PAGE_RETRY_DELAY_MS,
            ))
            .await;
        };

        let batch_len = resp.notes.len();
        for n in resp.notes {
            if n.position as usize >= target_leaves {
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

        offset += batch_len;
    }

    anyhow::ensure!(
        out.len() == expected_count,
        "Fetched {} notes but expected {} (start_offset={}, target_leaves={})",
        out.len(),
        expected_count,
        start_offset,
        target_leaves
    );
    Ok(out)
}
