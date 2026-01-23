use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use midnight_privacy::{Hash32, MerkleTree};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::provider::Provider;

/// Default commitment-tree depth used by Midnight Privacy.
pub const DEFAULT_TREE_DEPTH: u8 = 16;

const NOTES_PAGE_LIMIT: usize = 1000;
const SYNC_MAX_RETRIES: usize = 3;
const SYNC_RETRY_DELAY_MS: u64 = 200;

const POSITION_LOOKUP_MAX_RETRIES: usize = 10;
const POSITION_LOOKUP_RETRY_DELAY_MS: u64 = 200;

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
    depth: u8,
    state: RwLock<CachedTree>,
    sync_lock: Mutex<()>,
}

impl CommitmentTreeSyncer {
    pub fn new(depth: u8) -> Self {
        Self {
            depth,
            state: RwLock::new(CachedTree::new(depth)),
            sync_lock: Mutex::new(()),
        }
    }

    /// Sync the local cached tree to (at least) the current `/tree/state`.
    ///
    /// This is safe to call concurrently: only one task will perform network fetches and
    /// tree updates at a time.
    pub async fn sync_to_latest(&self, provider: &Provider) -> Result<()> {
        let _guard = self.sync_lock.lock().await;

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

            if let Some(chain_depth) = state.depth {
                anyhow::ensure!(
                    chain_depth == self.depth,
                    "Unexpected commitment tree depth: chain={}, client={}",
                    chain_depth,
                    self.depth
                );
            }

            let mut expected_root = [0u8; 32];
            expected_root.copy_from_slice(&state.root);
            let expected_next = state.next_position;

            // Quick no-op check before doing any paging.
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
                *st = CachedTree::new(self.depth);
            }

            // Try incremental sync first; on mismatch, fall back to a full rebuild.
            let incremental = self
                .try_incremental_sync(provider, expected_next, expected_root)
                .await?;
            if let Some(stats) = incremental {
                if stats.root_match {
                    let elapsed_ms = started.elapsed().as_millis();
                    tracing::info!(
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
            }

            match self
                .full_rebuild(provider, expected_next, expected_root)
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
                    tracing::info!(
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
                    *st = CachedTree::new(self.depth);
                    drop(st);
                    tokio::time::sleep(std::time::Duration::from_millis(SYNC_RETRY_DELAY_MS))
                        .await;
                    continue;
                }
            }
        }

        anyhow::bail!("Failed to sync commitment tree after {} attempts", SYNC_MAX_RETRIES);
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

        for attempt in 0..=POSITION_LOOKUP_MAX_RETRIES {
            self.sync_to_latest(provider).await?;

            let st = self.state.read().await;
            let mut positions = Vec::with_capacity(cms.len());
            let mut siblings = Vec::with_capacity(cms.len());

            let mut missing = false;
            for cm in cms {
                match st.pos_by_cm.get(cm).copied() {
                    Some(pos) => {
                        positions.push(pos);
                        siblings.push(st.tree.open(pos as usize));
                    }
                    None => {
                        missing = true;
                        break;
                    }
                }
            }

            if !missing {
                return Ok((st.tree.root(), positions, siblings));
            }

            if attempt == POSITION_LOOKUP_MAX_RETRIES {
                break;
            }

            drop(st);
            tokio::time::sleep(std::time::Duration::from_millis(
                POSITION_LOOKUP_RETRY_DELAY_MS,
            ))
            .await;
        }

        anyhow::bail!("Failed to resolve some commitment positions in the local tree cache");
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
    ) -> Result<FullRebuildStats> {
        let tree_init_started = Instant::now();
        let mut tree = MerkleTree::new(self.depth);
        let tree_init_ms = tree_init_started.elapsed().as_millis();

        let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();

        let mut fetched_notes = 0usize;
        let mut apply_ms = 0u128;
        if expected_next > 0 {
            tree.grow_to_fit(expected_next as usize);
            let fetch_started = Instant::now();
            let notes = fetch_notes(provider, 0, expected_next as usize).await?;
            let fetch_ms = fetch_started.elapsed().as_millis();

            fetched_notes = notes.len();
            let apply_started = Instant::now();
            for (pos, cm) in notes {
                if pos as usize >= tree.len() {
                    tree.grow_to_fit(pos as usize + 1);
                }
                tree.set_leaf(pos as usize, cm);
                pos_by_cm.insert(cm, pos);
            }
            apply_ms = apply_started.elapsed().as_millis();

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
    let mut out: Vec<(u64, Hash32)> = Vec::new();
    let mut offset = start_offset;

    while offset < target_leaves {
        let endpoint = format!(
            "/modules/midnight-privacy/notes?limit={}&offset={}",
            NOTES_PAGE_LIMIT, offset
        );
        let resp: NotesResp = provider
            .query_rest_endpoint(&endpoint)
            .await
            .with_context(|| format!("Failed to query notes batch at offset {}", offset))?;

        if resp.notes.is_empty() {
            break;
        }

        let batch_len = resp.notes.len();
        for n in resp.notes {
            if n.position as usize >= target_leaves {
                continue;
            }
            if n.commitment.len() != 32 {
                continue;
            }
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            out.push((n.position, cm));
        }

        offset += batch_len;
    }

    Ok(out)
}
