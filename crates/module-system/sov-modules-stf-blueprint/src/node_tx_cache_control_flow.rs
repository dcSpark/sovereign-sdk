//! Control flow used by the node to USE cached execution results from the sequencer.
//!
//! This module provides `NodeTxCacheControlFlow`, an implementation of `InjectedControlFlow`
//! that:
//! 1. Checks if a cached result exists for the transaction
//! 2. If cached, SKIPS execution and uses the cached result directly
//! 3. If not cached, executes normally
//!
//! This optimization allows the node to skip re-execution when the sequencer has already
//! computed and cached the result.

use std::marker::PhantomData;

use std::sync::Arc;

use sov_modules_api::{
    CacheVerificationResult, Context, DispatchCall, ExecutionContext, InjectedControlFlow,
    MaybeExecuted, PrecomputedResult, ProvisionalSequencerOutcome, Runtime, SlotGasMeter, Spec,
    StateCheckpoint, TransactionReceipt, TxControlFlow, TxScratchpad, GLOBAL_TX_CACHE,
};
use sov_rollup_interface::TxHash;
use sov_state::SlotValue;

/// Control flow used by the node (ExecutionContext::Node) to use cached execution
/// results from the sequencer.
///
/// When a cached result exists, this skips execution and uses the cached result directly.
/// When no cached result exists, it executes normally.
///
/// For non-node contexts, it behaves like `NoOpControlFlow`.
#[derive(Clone, Debug)]
pub struct NodeTxCacheControlFlow<S: Spec> {
    _marker: PhantomData<S>,
}

impl<S: Spec> Default for NodeTxCacheControlFlow<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Spec> NodeTxCacheControlFlow<S> {
    /// Create a new `NodeTxCacheControlFlow`.
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<S: Spec + 'static> InjectedControlFlow<S> for NodeTxCacheControlFlow<S> {
    fn try_warm_up_cache(&mut self, _scratchpad: &mut TxScratchpad<S, StateCheckpoint<S>>) {
        // No-op: node does not need cache warm-up.
    }

    fn should_skip_execution(&self, tx_hash: &TxHash) -> Option<Arc<PrecomputedResult<S>>> {
        // Check if we have a cached result from the sequencer
        let cached = GLOBAL_TX_CACHE.get::<S>(tx_hash);
        if let Some(ref result) = cached {
            tracing::debug!(
                tx_hash = %tx_hash,
                gas_used = ?result.gas_used,
                num_writes = result.tx_changes.writes.len(),
                num_events = result.receipt.events.len(),
                reward = ?result.reward,
                penalty = ?result.penalty,
                "[NODE CACHE HIT] Using cached result - SKIPPING EXECUTION"
            );
        }
        cached
    }

    fn on_verification_result(
        &self,
        tx_hash: &TxHash,
        computed: &PrecomputedResult<S>,
        verification_result: &CacheVerificationResult,
        execution_context: ExecutionContext,
    ) {
        // Only log in Node context
        if execution_context != ExecutionContext::Node {
            return;
        }

        match verification_result {
            CacheVerificationResult::CacheMiss => {
                tracing::debug!(
                    tx_hash = %tx_hash,
                    "[NODE CACHE] No cached result found for transaction"
                );
            }
            CacheVerificationResult::Match => {
                tracing::debug!(
                    tx_hash = %tx_hash,
                    gas_used = ?computed.gas_used,
                    num_writes = computed.tx_changes.writes.len(),
                    num_events = computed.receipt.events.len(),
                    reward = ?computed.reward,
                    penalty = ?computed.penalty,
                    "[NODE CACHE VERIFIED] ✓ Computed result matches cached result"
                );
            }
            CacheVerificationResult::Mismatch { reason } => {
                tracing::error!(
                    tx_hash = %tx_hash,
                    reason = %reason,
                    computed_gas = ?computed.gas_used,
                    computed_writes = computed.tx_changes.writes.len(),
                    computed_events = computed.receipt.events.len(),
                    computed_reward = ?computed.reward,
                    computed_penalty = ?computed.penalty,
                    "[NODE CACHE MISMATCH] ✗ Computed result differs from cached result!"
                );

                // Log detailed write-by-write comparison for debugging
                if let Some(cached) = GLOBAL_TX_CACHE.get::<S>(tx_hash) {
                    tracing::error!(
                        cached_gas = ?cached.gas_used,
                        cached_writes = cached.tx_changes.writes.len(),
                        cached_events = cached.receipt.events.len(),
                        cached_reward = ?cached.reward,
                        cached_penalty = ?cached.penalty,
                        "[NODE CACHE MISMATCH] Cached result details"
                    );

                    // Helper to format value for logging
                    fn format_value(val: &Option<SlotValue>) -> String {
                        match val {
                            None => "None".to_string(),
                            Some(v) => {
                                let bytes = v.value();
                                if bytes.len() > 64 {
                                    format!(
                                        "{}... ({} bytes)",
                                        hex::encode(&bytes[..32]),
                                        bytes.len()
                                    )
                                } else {
                                    hex::encode(bytes)
                                }
                            }
                        }
                    }

                    // Log each write for comparison
                    tracing::error!("[NODE CACHE MISMATCH] === COMPUTED WRITES (Node) ===");
                    for (i, ((key, ns), value)) in computed.tx_changes.writes.iter().enumerate() {
                        let key_str = String::from_utf8_lossy(key.as_ref());
                        tracing::error!(
                            idx = i,
                            key = %key_str,
                            namespace = ?ns,
                            value = %format_value(value),
                            "[COMPUTED WRITE]"
                        );
                    }

                    tracing::error!("[NODE CACHE MISMATCH] === CACHED WRITES (Sequencer) ===");
                    for (i, ((key, ns), value)) in cached.tx_changes.writes.iter().enumerate() {
                        let key_str = String::from_utf8_lossy(key.as_ref());
                        tracing::error!(
                            idx = i,
                            key = %key_str,
                            namespace = ?ns,
                            value = %format_value(value),
                            "[CACHED WRITE]"
                        );
                    }
                }
            }
        }
    }

    fn pre_flight<RT: Runtime<S>>(
        &self,
        _runtime: &RT,
        _context: &Context<S>,
        _call: &<RT as DispatchCall>::Decodable,
    ) -> TxControlFlow<()> {
        // Node is purely replaying; no extra pre-flight logic.
        TxControlFlow::ContinueProcessing(())
    }

    fn post_tx(
        &self,
        provisional_outcome: ProvisionalSequencerOutcome<S>,
        dirty_scratchpad: TxScratchpad<S, StateCheckpoint<S>>,
        _slot_gas_meter_before_tx: &SlotGasMeter<S>,
        _gas_used: &<S as Spec>::Gas,
        _exec_context: ExecutionContext,
    ) -> (StateCheckpoint<S>, TxControlFlow<TransactionReceipt<S>>) {
        match provisional_outcome.execution_status {
            MaybeExecuted::Executed(receipt) => (
                dirty_scratchpad.commit(),
                TxControlFlow::ContinueProcessing(receipt),
            ),
            MaybeExecuted::SequencerOutOfFunds(_) => {
                // For node replay, an "out of funds" here should not change state:
                (dirty_scratchpad.commit(), TxControlFlow::IgnoreTx)
            }
        }
    }
}
