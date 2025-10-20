#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

//! This module demonstrates privacy-preserving transactions using Ligero ZK proofs with Sovereign SDK.
//! It implements a shielded pool with note commitments, nullifiers, and Merkle tree for membership proofs.

mod call;
mod event;
mod genesis;
mod hash;
mod merkle;
mod types;
pub mod viewing;

pub use call::CallMessage;
pub use event::Event;
pub use genesis::*;
pub use hash::*;
pub use merkle::*;
pub use types::*;
pub use viewing::{decrypt_and_verify_note, encrypt_note_for_fvk};

use sov_modules_api::{
    Context, DaSpec, GenesisState, Module, ModuleId, ModuleInfo, ModuleRestApi, Spec, StateMap,
    StateValue, TxState,
};
use std::collections::VecDeque;

#[cfg(feature = "native")]
use std::sync::{Arc, RwLock};

#[cfg(feature = "native")]
use once_cell::sync::Lazy;

/// Global database connection for proof caching.
/// This is set by the sequencer at startup and accessed by the midnight-privacy module
/// to check for cached proof verifications before running expensive Ligero verification.
#[cfg(feature = "native")]
static PROOF_CACHE_DB: Lazy<RwLock<Option<Arc<sea_orm::DatabaseConnection>>>> =
    Lazy::new(|| RwLock::new(None));

/// Set the global database connection for proof caching.
/// This should be called by the sequencer during initialization.
#[cfg(feature = "native")]
pub fn set_proof_cache_db(db: Arc<sea_orm::DatabaseConnection>) {
    if let Ok(mut lock) = PROOF_CACHE_DB.write() {
        *lock = Some(db);
    }
}

/// Get a clone of the global database connection for proof caching, if available.
#[cfg(feature = "native")]
pub fn get_proof_cache_db() -> Option<Arc<sea_orm::DatabaseConnection>> {
    PROOF_CACHE_DB.read().ok()?.clone()
}

/// MidnightPrivacy module: A privacy-preserving shielded pool using Ligero ZK proofs.
///
/// This module allows users to:
/// 1. Deposit tokens into the pool and create note commitments
/// 2. Spend notes by providing ZK proofs that demonstrate:
///    - Knowledge of a note in the tree
///    - A valid Merkle path to an anchor root
///    - Proper nullifier derivation
/// 3. Withdraw tokens from the pool with proof-bound amounts
///
/// The nullifier prevents double-spending, and the anchor root window allows
/// parallel transactions while maintaining security.
///
/// # Module State
/// - `commitment_tree`: Merkle tree of note commitments
/// - `next_position`: Next available position in the tree
/// - `nullifier_set`: Set of used nullifiers (prevents double-spending)
/// - `recent_roots`: Recent Merkle roots (anchor window for fast mempool checks)
/// - `root_window_size`: Size of the anchor window
/// - `all_roots`: Persistent index of ALL historical roots (NOMT-backed, enables long-range anchors)
/// - `root_seq`: Monotonic sequence counter for root ordering
/// - `method_id`: Ligero method ID (code commitment) for proof verification
/// - `admin`: Administrator who can update the method ID
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

    /// Domain tag used in all note/hash derivations.
    #[state]
    pub domain: StateValue<Hash32>,

    /// Single supported token (native).
    #[state]
    pub token_id: StateValue<sov_bank::TokenId>,

    /// Bank module to hold/transfer the native token.
    #[module]
    pub bank: sov_bank::Bank<S>,
}

impl<S: Spec> Module for ValueMidnightPrivacy<S> {
    type Spec = S;

    type Config = ValueSetterZkConfig<S>;

    type CallMessage = CallMessage<S>;

    type Event = Event;

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
            CallMessage::CreateNote { note, gas } => {
                Ok(self.create_note(note, gas, context, state)?)
            }
            CallMessage::SpendNote { proof, gas } => {
                Ok(self.spend_note(proof, gas, context, state)?)
            }
            CallMessage::Deposit {
                amount,
                rho,
                recipient,
                gas,
            } => Ok(self.deposit(amount, rho, recipient, gas, context, state)?),
            CallMessage::Withdraw {
                proof,
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                gas,
            } => Ok(self.withdraw(
                proof,
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                gas,
                context,
                state,
            )?),
            CallMessage::UpdateMethodId { new_method_id } => {
                Ok(self.update_method_id(new_method_id, context, state)?)
            }
        };

        // Commit the state changes if successful
        state_wrapped.commit();
        res
    }
}
