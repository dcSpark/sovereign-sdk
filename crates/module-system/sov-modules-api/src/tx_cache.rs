//! Global in-process execution cache for transaction results.
//!
//! This module provides a process-local, cross-rollup execution cache that stores
//! precomputed transaction results. The cache is keyed by `(Spec type, tx_hash)`.
//!
//! ## Usage
//!
//! The sequencer populates the cache when it executes transactions. The node, when
//! replaying DA blocks, can look up cached results and use them directly to skip
//! re-execution, or verify them against its own execution.
//!
//! ## Performance
//!
//! The cache stores `Arc<PrecomputedResult<S>>` to avoid cloning large structures
//! on every cache hit. This means cache lookups only bump a reference count.

use std::any::TypeId;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use once_cell::sync::Lazy;
use sov_rollup_interface::TxHash;

use crate::{Amount, Spec, TransactionReceipt, TxChangeSet};

/// Default max number of cached txs across all rollups in this process.
/// Tune as needed or make it configurable via env.
const DEFAULT_TX_CACHE_CAPACITY: usize = 10_000;

/// A precomputed transaction result that can be cached and verified.
#[derive(Debug, Clone)]
pub struct PrecomputedResult<S: Spec> {
    /// The transaction receipt.
    pub receipt: TransactionReceipt<S>,
    /// The state changes caused by this transaction.
    pub tx_changes: TxChangeSet,
    /// The gas used by this transaction.
    pub gas_used: S::Gas,
    /// Execution time in microseconds.
    pub execution_time_micros: u64,
    /// The reward earned by the sequencer for this transaction.
    pub reward: Amount,
    /// The penalty incurred by the sequencer for this transaction.
    pub penalty: Amount,
}

/// A simple process-local, cross-rollup execution cache:
/// key = (Spec type, tx_hash), value = Arc<PrecomputedResult<S>>.
///
/// This is intentionally *best effort* and only used as a local optimization.
/// Other nodes still fully re-execute.
///
/// Uses `Arc` to avoid cloning large structures on cache hits - lookups only
/// bump a reference count.
pub struct GlobalTxExecutionCache {
    map: DashMap<(TypeId, TxHash), Arc<dyn std::any::Any + Send + Sync>>,
    order: Mutex<VecDeque<(TypeId, TxHash)>>,
    capacity: usize,
}

impl GlobalTxExecutionCache {
    /// Create a new cache with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            map: DashMap::new(),
            order: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    /// Insert or update a cached result.
    pub fn insert<S: Spec + 'static>(&self, tx_hash: TxHash, precomputed: PrecomputedResult<S>) {
        let key = (TypeId::of::<S>(), tx_hash);
        self.map.insert(key, Arc::new(precomputed));
        let mut order = self.order.lock().unwrap();
        order.push_back(key);
        if order.len() > self.capacity {
            if let Some(old_key) = order.pop_front() {
                self.map.remove(&old_key);
            }
        }
    }

    /// Fetch a cached result for this Spec and tx hash.
    /// Returns an Arc to avoid cloning the entire PrecomputedResult.
    pub fn get<S: Spec + 'static>(&self, tx_hash: &TxHash) -> Option<Arc<PrecomputedResult<S>>> {
        let key = (TypeId::of::<S>(), *tx_hash);
        self.map.get(&key).and_then(|entry| {
            // entry is a DashMap Ref which derefs to Arc<dyn Any + Send + Sync>
            // Clone the Arc (cheap - just bumps refcount) and downcast
            let any_arc: Arc<dyn std::any::Any + Send + Sync> = Arc::clone(&*entry);
            any_arc.downcast::<PrecomputedResult<S>>().ok()
        })
    }

    /// Remove a specific entry from the cache.
    pub fn remove<S: Spec + 'static>(&self, tx_hash: &TxHash) {
        let key = (TypeId::of::<S>(), *tx_hash);
        self.map.remove(&key);
        // Note: We don't remove from `order` for simplicity; it will be cleaned up
        // when it reaches the front of the queue.
    }

    /// Optional helper if you ever want to force a flush (e.g. on reorg).
    pub fn clear_all(&self) {
        self.map.clear();
        let mut order = self.order.lock().unwrap();
        order.clear();
    }

    /// Get the current number of cached entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Get the current size of the order queue (for debugging).
    /// Note: This may differ from `len()` due to stale entries.
    pub fn order_queue_len(&self) -> usize {
        self.order.lock().unwrap().len()
    }

    /// Get cache capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

// Safety: DashMap and Mutex are thread-safe
unsafe impl Send for GlobalTxExecutionCache {}
unsafe impl Sync for GlobalTxExecutionCache {}

/// Global, process-local cache.
pub static GLOBAL_TX_CACHE: Lazy<GlobalTxExecutionCache> =
    Lazy::new(|| GlobalTxExecutionCache::new(DEFAULT_TX_CACHE_CAPACITY));

/// Result of comparing a computed result with a cached result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheVerificationResult {
    /// No cached result was found for this transaction.
    CacheMiss,
    /// The computed result matches the cached result.
    Match,
    /// The computed result differs from the cached result.
    Mismatch {
        /// Description of what differed.
        reason: String,
    },
}

impl CacheVerificationResult {
    /// Returns true if the result indicates a match.
    pub fn is_match(&self) -> bool {
        matches!(self, CacheVerificationResult::Match)
    }

    /// Returns true if the result indicates a mismatch.
    pub fn is_mismatch(&self) -> bool {
        matches!(self, CacheVerificationResult::Mismatch { .. })
    }

    /// Returns true if there was no cached result.
    pub fn is_cache_miss(&self) -> bool {
        matches!(self, CacheVerificationResult::CacheMiss)
    }
}

/// Verify a computed result against the cache.
///
/// This function compares the computed transaction result against a cached version
/// (if available). It returns a verification result indicating whether:
/// - There was no cached result (CacheMiss)
/// - The results match (Match)
/// - The results differ (Mismatch)
///
/// ## What is compared
///
/// - Transaction hash
/// - Receipt success/failure status
/// - Gas used
/// - Reward and penalty amounts
/// - Events count and content
/// - ALL write keys and values
pub fn verify_against_cache<S: Spec + 'static>(
    tx_hash: &TxHash,
    computed: &PrecomputedResult<S>,
) -> CacheVerificationResult {
    let cached = match GLOBAL_TX_CACHE.get::<S>(tx_hash) {
        Some(c) => c,
        None => return CacheVerificationResult::CacheMiss,
    };

    // Compare transaction hashes
    if computed.receipt.tx_hash != cached.receipt.tx_hash {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Transaction hash mismatch: computed={}, cached={}",
                computed.receipt.tx_hash, cached.receipt.tx_hash
            ),
        };
    }

    // Compare receipt success status
    let computed_success = computed.receipt.receipt.is_successful();
    let cached_success = cached.receipt.receipt.is_successful();
    if computed_success != cached_success {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Receipt success mismatch: computed={}, cached={}",
                computed_success, cached_success
            ),
        };
    }

    // Compare gas used
    if computed.gas_used != cached.gas_used {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Gas used mismatch: computed={:?}, cached={:?}",
                computed.gas_used, cached.gas_used
            ),
        };
    }

    // Compare reward
    if computed.reward != cached.reward {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Reward mismatch: computed={:?}, cached={:?}",
                computed.reward, cached.reward
            ),
        };
    }

    // Compare penalty
    if computed.penalty != cached.penalty {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Penalty mismatch: computed={:?}, cached={:?}",
                computed.penalty, cached.penalty
            ),
        };
    }

    // Compare events count
    if computed.receipt.events.len() != cached.receipt.events.len() {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Events count mismatch: computed={}, cached={}",
                computed.receipt.events.len(),
                cached.receipt.events.len()
            ),
        };
    }

    // Compare events content
    for (i, (computed_event, cached_event)) in computed
        .receipt
        .events
        .iter()
        .zip(cached.receipt.events.iter())
        .enumerate()
    {
        if computed_event != cached_event {
            return CacheVerificationResult::Mismatch {
                reason: format!("Event mismatch at index {}", i),
            };
        }
    }

    // Compare ALL writes (no filtering)
    // Compare number of writes first (fast path)
    if computed.tx_changes.writes.len() != cached.tx_changes.writes.len() {
        return CacheVerificationResult::Mismatch {
            reason: format!(
                "Write count mismatch: computed={}, cached={}",
                computed.tx_changes.writes.len(),
                cached.tx_changes.writes.len()
            ),
        };
    }

    // Create sorted index vectors instead of cloning the entire writes
    let mut computed_indices: Vec<usize> = (0..computed.tx_changes.writes.len()).collect();
    let mut cached_indices: Vec<usize> = (0..cached.tx_changes.writes.len()).collect();

    computed_indices.sort_by(|&a, &b| {
        computed.tx_changes.writes[a]
            .0
            .cmp(&computed.tx_changes.writes[b].0)
    });
    cached_indices.sort_by(|&a, &b| {
        cached.tx_changes.writes[a]
            .0
            .cmp(&cached.tx_changes.writes[b].0)
    });

    for (i, (&computed_idx, &cached_idx)) in computed_indices
        .iter()
        .zip(cached_indices.iter())
        .enumerate()
    {
        let computed_write = &computed.tx_changes.writes[computed_idx];
        let cached_write = &cached.tx_changes.writes[cached_idx];

        if computed_write.0 != cached_write.0 {
            let computed_key_str = String::from_utf8_lossy(computed_write.0 .0.as_ref());
            let cached_key_str = String::from_utf8_lossy(cached_write.0 .0.as_ref());
            return CacheVerificationResult::Mismatch {
                reason: format!(
                    "Write key mismatch at index {}: computed='{}', cached='{}'",
                    i, computed_key_str, cached_key_str
                ),
            };
        }
        if computed_write.1 != cached_write.1 {
            let key_str = String::from_utf8_lossy(computed_write.0 .0.as_ref());
            return CacheVerificationResult::Mismatch {
                reason: format!("Write value mismatch at index {} for key '{}'", i, key_str),
            };
        }
    }

    CacheVerificationResult::Match
}

/// Statistics for batch-level verification.
/// Tracks the results of per-transaction verification within a batch.
#[derive(Debug, Clone, Default)]
pub struct BatchVerificationStats {
    /// Number of transactions where cache was used (skipped execution, no verification)
    pub cache_hits_used: usize,
    /// Number of transactions verified successfully (re-executed and matched cache)
    pub verified_matches: usize,
    /// Number of transactions with cache misses (executed, nothing to verify against)
    pub cache_misses: usize,
    /// Number of transactions with mismatches (executed, differed from cache)
    pub mismatches: usize,
    /// Total number of transactions in the batch
    pub total_txs: usize,
    /// Total number of writes applied/compared
    pub total_writes: usize,
}

impl BatchVerificationStats {
    /// Create a new empty stats tracker
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a cache hit where we used the cached result (no verification)
    pub fn record_cache_hit(&mut self, write_count: usize) {
        self.total_txs += 1;
        self.total_writes += write_count;
        self.cache_hits_used += 1;
    }

    /// Record a verification result (for txs that were actually executed)
    pub fn record(&mut self, result: &CacheVerificationResult, write_count: usize) {
        self.total_txs += 1;
        self.total_writes += write_count;
        match result {
            CacheVerificationResult::Match => self.verified_matches += 1,
            CacheVerificationResult::CacheMiss => self.cache_misses += 1,
            CacheVerificationResult::Mismatch { .. } => self.mismatches += 1,
        }
    }

    /// Check if all verified transactions matched (no mismatches)
    pub fn all_verified_match(&self) -> bool {
        self.mismatches == 0
    }

    /// Get the verification success rate (for txs that were actually verified)
    pub fn success_rate(&self) -> f64 {
        let verified = self.verified_matches + self.mismatches;
        if verified == 0 {
            1.0
        } else {
            self.verified_matches as f64 / verified as f64
        }
    }
}

impl std::fmt::Display for BatchVerificationStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BatchVerification {{ total: {}, cache_hits: {}, verified: {}, mismatches: {}, cache_misses: {}, writes: {}, success_rate: {:.1}% }}",
            self.total_txs,
            self.cache_hits_used,
            self.verified_matches,
            self.mismatches,
            self.cache_misses,
            self.total_writes,
            self.success_rate() * 100.0
        )
    }
}

/// Count the total number of writes for a given PrecomputedResult.
pub fn count_writes<S: Spec>(computed: &PrecomputedResult<S>) -> usize {
    computed.tx_changes.writes.len()
}
