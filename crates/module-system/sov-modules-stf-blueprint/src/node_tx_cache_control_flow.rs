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
        GLOBAL_TX_CACHE.get::<S>(tx_hash)
    }

    fn on_verification_result(
        &self,
        tx_hash: &TxHash,
        computed: &PrecomputedResult<S>,
        verification_result: &CacheVerificationResult,
        execution_context: ExecutionContext,
    ) {
        // Only log errors in Node context
        if execution_context != ExecutionContext::Node {
            return;
        }

        // Only log mismatches - they indicate potential issues
        if let CacheVerificationResult::Mismatch { reason } = verification_result {
            tracing::error!(
                tx_hash = %tx_hash,
                reason = %reason,
                computed_gas = ?computed.gas_used,
                computed_writes = computed.tx_changes.writes.len(),
                "[NODE CACHE MISMATCH] Computed result differs from cached result"
            );
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
