#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

//! This module demonstrates privacy-preserving transactions using Ligero ZK proofs with Sovereign SDK.
//! It implements a shielded pool with note commitments, nullifiers, and Merkle tree for membership proofs.

mod call;
mod event;
mod genesis;
mod hash;
mod merkle;
mod preverified;
#[cfg(feature = "native")]
mod query;
mod types;
pub mod viewing;

pub use call::CallMessage;
pub use event::Event;
pub use genesis::*;
pub use hash::*;
pub use merkle::*;
pub use preverified::{
    cache_pre_verified_spend, clear_pre_verified_spend, get_pre_verified_spend,
    prime_pre_verified_spend,
};
pub use types::*;
pub use viewing::{
    ct_hash, decrypt_and_verify_note, decrypt_and_verify_note_with_sender, encrypt_note_for_fvk,
    encrypt_note_for_fvk_with_sender, fvk_commitment, view_kdf, view_mac,
};

#[cfg(feature = "native")]
pub use query::*;

use sov_modules_api::capabilities::RollupHeight;
use sov_modules_api::hooks::BlockHooks;
use sov_modules_api::VersionReader;
use sov_modules_api::{
    Context, DaSpec, GenesisState, Module, ModuleId, ModuleInfo, ModuleRestApi, Spec, StateMap,
    StateValue, TxState,
};
use std::collections::VecDeque;

pub use crate::hash::{Hash32, PendingCommitmentKey, PendingNullifierKey, PendingRootKey, RootKey};

/// MidnightPrivacy module: A privacy-preserving shielded pool using Ligero ZK proofs.
///
/// This module provides three core operations for a Zcash-style shielded pool:
///
/// 1. **Deposit**: Put money INTO the pool (Transparent → Shielded)
///    - Transfers transparent tokens from sender
///    - Creates a note commitment in the tree
///
/// 2. **Transfer**: Move money WITHIN the pool (Shielded → Shielded)
///    - Verifies ZK proof
///    - Consumes input note by nullifier
///    - Creates output note commitments (pure privacy)
///    - All value stays shielded
///
/// 3. **Withdraw**: Take money OUT of the pool (Shielded → Transparent)
///    - Verifies ZK proof
///    - Consumes input note by nullifier
///    - Creates output commitments (for change)
///    - Transfers transparent tokens to recipient
///
/// The ZK proofs demonstrate:
/// - Knowledge of a note in the Merkle tree (via authentication path)
/// - Proper nullifier derivation (prevents double-spending)
/// - Value conservation: input_value = sum(output_values) + withdraw_amount
///
/// The nullifier prevents double-spending, and the anchor root window allows
/// parallel transactions while maintaining security.
///
/// # Module State
/// - `commitment_tree`: Merkle tree of note commitments
/// - `next_position`: Next available position in the commitment tree
/// - `nullifier_tree`: Merkle tree of spent nullifiers (Aztec-style dual-tree design)
/// - `next_nullifier_position`: Next available position in the nullifier tree
/// - `nullifier_set`: Set of used nullifiers (prevents double-spending, O(1) lookup)
/// - `recent_roots`: Recent Merkle roots (anchor window for fast mempool checks)
/// - `root_window_size`: Size of the anchor window
/// - `all_roots`: Persistent index of ALL historical roots (NOMT-backed, enables long-range anchors)
/// - `root_seq`: Monotonic sequence counter for root ordering
/// - `method_id`: Ligero method ID (code commitment) for proof verification
/// - `admin`: Administrator who can update the method ID
/// - `blacklist_root`: Deny-map Merkle root (freeze/blacklist primitive)
/// - `blacklist_buckets`: Deny-map bucket entries (non-empty buckets)
/// - `blacklist_nodes`: Deny-map Merkle nodes (non-default)
/// - `pool_admins`: Addresses allowed to update `blacklist_root` (membership map)
/// - `pool_admin_list`: Sorted list of pool admins (for queries)
/// - `domain`: Domain tag for all note/hash operations
/// - `token_id`: The single supported native token
/// - `bank`: Bank module for token transfers
///
/// # Derives
/// - `ModuleInfo`: Required for all modules
/// - `ModuleRestApi`: Automatically generates REST API endpoints
#[derive(Clone, ModuleInfo, ModuleRestApi)]
pub struct ValueMidnightPrivacy<S: Spec> {
    /// The ID of the module.
    #[id]
    pub id: ModuleId,

    /// Merkle tree of note commitments.
    #[state]
    pub commitment_tree: StateValue<MerkleTree>,

    /// Next available position in the commitment tree.
    #[state]
    pub next_position: StateValue<u64>,

    /// Merkle tree of spent nullifiers (Aztec-style dual-tree design).
    /// Append-only: each new nullifier is inserted at the next free position.
    /// This tree is maintained in parallel with `nullifier_set` for future
    /// IMT-based non-membership proofs in the circuit.
    #[state]
    pub nullifier_tree: StateValue<MerkleTree>,

    /// Next available position in the nullifier tree.
    #[state]
    pub next_nullifier_position: StateValue<u64>,

    /// Set of used nullifiers (maps nullifier -> true if spent).
    #[state]
    pub nullifier_set: StateMap<NullifierKey, bool>,

    /// Recent Merkle roots (circular buffer for anchor window).
    /// Uses VecDeque for O(1) insertion and removal at both ends.
    /// This provides fast mempool checks for recent transactions.
    #[state]
    pub recent_roots: StateValue<VecDeque<Hash32>>,

    /// Size of the recent roots window.
    #[state]
    pub root_window_size: StateValue<u32>,

    /// Persistent index of **all** Merkle roots ever produced by this module.
    /// The map is NOMT-backed (consensus state), so membership proofs are cheap and permanent.
    /// This enables long-range anchor validation: any historical root remains valid forever.
    /// Key: RootKey(root), Value: monotonically increasing sequence number (first-seen order).
    #[state]
    pub all_roots: StateMap<RootKey, u64>,

    /// Next sequence number to assign when recording a new root in `all_roots`.
    /// Increments monotonically for each unique root.
    #[state]
    pub root_seq: StateValue<u64>,

    /// Code commitment (32 bytes) of the Ligero guest program that verifies spend proofs.
    /// This is the SHA-256 hash of (WASM program bytes || packing parameter).
    #[state]
    pub method_id: StateValue<[u8; 32]>,

    /// Administrator address who can update the method ID.
    #[state]
    pub admin: StateValue<S::Address>,

    /// Deny-map Merkle root used by the ZK circuits to enforce address freezing.
    ///
    /// The spend (and deposit) circuits take `blacklist_root` as a public input and prove that
    /// selected identities are not blacklisted under this root.
    #[state]
    pub blacklist_root: StateValue<Hash32>,

    /// Deny-map bucket entries stored per leaf position (sparse: only non-empty buckets stored).
    #[state]
    pub blacklist_buckets: StateMap<u64, crate::hash::BlacklistBucketEntries>,

    /// Deny-map Merkle nodes (sparse: only non-default nodes stored).
    ///
    /// This supports efficient Merkle opening queries for clients constructing ZK proofs.
    #[state]
    pub blacklist_nodes: StateMap<BlacklistNodeKey, Hash32>,

    /// Set of pool admin addresses allowed to update `blacklist_root`.
    #[state]
    pub pool_admins: StateMap<S::Address, bool>,

    /// Sorted list of pool admins (for enumeration in REST queries).
    #[state]
    pub pool_admin_list: StateValue<Vec<S::Address>>,

    /// Domain tag used in all note/hash derivations.
    #[state]
    pub domain: StateValue<Hash32>,

    /// Single supported token (native).
    #[state]
    pub token_id: StateValue<sov_bank::TokenId>,

    /// Total amount deposited into the pool (transparent → shielded).
    #[state]
    pub total_deposited: StateValue<u128>,

    /// Total number of deposits.
    #[state]
    pub deposit_count: StateValue<u64>,

    /// Total amount withdrawn from the pool (shielded → transparent).
    #[state]
    pub total_withdrawn: StateValue<u128>,

    /// Total number of withdrawals.
    #[state]
    pub withdraw_count: StateValue<u64>,

    /// Indexed pending roots: (rollup_height, idx) -> root.
    /// Each block appends roots with sequential indices, avoiding VecDeque rewrite overhead.
    /// For thousands of txs per block, this is O(1) per append vs O(n) for VecDeque serialization.
    ///
    /// ASSUMPTION: rollup_height_to_access() is stable throughout block execution (start to end_hook).
    /// DANGER: Stale entries from abandoned blocks (crashes/reverts) accumulate but are harmless
    /// (never read, don't affect correctness, minimal state cost). Cleanup not implemented.
    #[state]
    pub pending_roots_indexed: StateMap<PendingRootKey, Hash32>,

    /// Per-height counter: how many roots are pending for each height.
    /// Used to know how many indices to iterate when flushing.
    #[state]
    pub pending_roots_count: StateMap<RollupHeight, u32>,

    /// Parallel-safe pending commitments storage: (rollup_height, commitment) -> ().
    /// Uses the commitment hash as the key to ensure uniqueness across parallel executions.
    /// Each transaction writes to a unique key, so no conflicts occur during parallel execution.
    /// The value is just a presence marker - positions are assigned at flush time.
    /// Enumerated at flush time using StateMap::iter_prefix with PendingCommitmentPrefix.
    #[state]
    pub pending_commitments_by_hash: StateMap<PendingCommitmentKey, ()>,

    /// Parallel-safe pending nullifiers storage: (rollup_height, nullifier) -> ().
    /// Uses the nullifier hash as the key to ensure uniqueness across parallel executions.
    /// Enumerated at flush time using StateMap::iter_prefix with PendingNullifierPrefix.
    #[state]
    pub pending_nullifiers_by_hash: StateMap<PendingNullifierKey, ()>,

    /// Total number of **spent nullifiers** (both transfers and withdrawals).
    #[state]
    pub spent_nullifier_count: StateValue<u64>,

    /// Bank module to hold/transfer the native token.
    #[module]
    pub bank: sov_bank::Bank<S>,
}

impl<S: Spec> Module for ValueMidnightPrivacy<S> {
    type Spec = S;

    type Config = MidnightPrivacyConfig<S>;

    type CallMessage = CallMessage<S>;

    type Event = Event<S>;

    fn genesis(
        &mut self,
        _genesis_rollup_header: &<<S as Spec>::Da as DaSpec>::BlockHeader,
        config: &Self::Config,
        state: &mut impl GenesisState<S>,
    ) -> anyhow::Result<()> {
        // Initialize the module with genesis configuration
        self.init_module(config, state)
    }

    fn call(
        &mut self,
        msg: Self::CallMessage,
        context: &Context<Self::Spec>,
        state: &mut impl TxState<S>,
    ) -> anyhow::Result<()> {
        // Use a revertable state wrapper to ensure atomicity
        let mut state_wrapped = state.to_revertable();
        let state = &mut state_wrapped;

        let res = match msg {
            CallMessage::Deposit {
                amount,
                rho,
                recipient,
                view_fvks,
                gas,
            } => Ok(self.deposit(amount, rho, recipient, view_fvks, gas, context, state)?),
            CallMessage::Transfer {
                proof,
                anchor_root,
                nullifier,
                view_ciphertexts,
                gas,
            } => Ok(self.transfer(
                proof,
                anchor_root,
                nullifier,
                view_ciphertexts,
                gas,
                context,
                state,
            )?),
            CallMessage::Withdraw {
                proof,
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                view_ciphertexts,
                gas,
            } => Ok(self.withdraw(
                proof,
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                view_ciphertexts,
                gas,
                context,
                state,
            )?),
            CallMessage::UpdateMethodId { new_method_id } => {
                Ok(self.update_method_id(new_method_id, context, state)?)
            }
            CallMessage::FreezeAddress { address } => {
                Ok(self.freeze_address(address, context, state)?)
            }
            CallMessage::UnfreezeAddress { address } => {
                Ok(self.unfreeze_address(address, context, state)?)
            }
            CallMessage::AddPoolAdmin { admin } => Ok(self.add_pool_admin(admin, context, state)?),
            CallMessage::RemovePoolAdmin { admin } => {
                Ok(self.remove_pool_admin(admin, context, state)?)
            }
        };

        // Commit the state changes if successful
        state_wrapped.commit();
        res
    }
}

/// Implement BlockHooks to flush pending roots at the end of each block.
/// This enables true parallelism within blocks by deferring root publication
/// until all transactions have been executed.
impl<S: Spec> BlockHooks for ValueMidnightPrivacy<S> {
    type Spec = S;

    fn begin_rollup_block_hook(
        &mut self,
        _visible_hash: &<<Self::Spec as Spec>::Storage as sov_modules_api::Storage>::Root,
        state: &mut sov_modules_api::StateCheckpoint<Self::Spec>,
    ) {
        // Reset pending roots counter for current height (defensive against re-execution).
        let height = state.rollup_height_to_access();
        let _ = self.pending_roots_count.set(&height, &0u32, state);
        // Note: Slot-based commitment/nullifier storage doesn't need counter resets.
        // Each slot is overwritten per-block, and enumeration scans all slots.
    }

    fn end_rollup_block_hook(&mut self, state: &mut sov_modules_api::StateCheckpoint<Self::Spec>) {
        // Flush all pending roots into recent_roots and all_roots
        // This should never fail in normal operation
        if let Err(e) = self.end_block_flush(state) {
            panic!("FATAL: MidnightPrivacy end_block_flush failed: {}", e);
        }
    }
}
