#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

//! This module demonstrates privacy-preserving transactions using Ligero ZK proofs with Sovereign SDK.
//! It implements a shielded pool with note commitments, nullifiers, and Merkle tree for membership proofs.

mod call;
mod genesis;
mod event;
mod hash;
mod merkle;
mod types;

pub use call::CallMessage;
pub use event::Event;
pub use genesis::*;
pub use hash::*;
pub use merkle::*;
pub use types::*;

use std::collections::VecDeque;
use sov_modules_api::{
    Context, DaSpec, GenesisState, Module, ModuleId, ModuleInfo, ModuleRestApi, Spec, 
    StateMap, StateValue, TxState,
};

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
/// - `recent_roots`: Recent Merkle roots (anchor window)
/// - `root_window_size`: Size of the anchor window
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
    #[state]
    pub recent_roots: StateValue<VecDeque<Hash32>>,

    /// Size of the recent roots window.
    #[state]
    pub root_window_size: StateValue<u32>,

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
            CallMessage::Deposit { amount, rho, recipient, gas } => {
                Ok(self.deposit(amount, rho, recipient, gas, context, state)?)
            }
            CallMessage::Withdraw { proof, anchor_root, nullifier, withdraw_amount, to, gas } => {
                Ok(self.withdraw(proof, anchor_root, nullifier, withdraw_amount, to, gas, context, state)?)
            }
            CallMessage::UpdateMethodId { new_method_id } => {
                Ok(self.update_method_id(new_method_id, context, state)?)
            }
        };
        
        // Commit the state changes if successful
        state_wrapped.commit();
        res
    }
}

