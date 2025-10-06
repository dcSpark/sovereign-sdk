#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

//! This module demonstrates integration of Ligetron ZK proofs with Sovereign SDK.
//! Users must provide a valid proof alongside the value they wish to set.

mod call;
mod genesis;
mod event;

pub use call::*;
pub use event::Event;
pub use genesis::*;

use sov_modules_api::{
    Context, DaSpec, GenesisState, Module, ModuleId, ModuleInfo, ModuleRestApi, Spec, StateValue,
    TxState,
};

/// ValueSetterZk module: Sets a value only if a valid Ligetron ZK proof is provided.
///
/// The proof must demonstrate that the value meets certain constraints (enforced by the guest program).
/// For this implementation, the guest program verifies that the value is within [0, 100].
///
/// # Module State
/// - `value`: The current value (u32)
/// - `method_id`: Ligetron method ID (code commitment) for proof verification
/// - `admin`: Administrator who can update the method ID
///
/// # Derives
/// - `ModuleInfo`: Required for all modules
/// - `ModuleRestApi`: Automatically generates REST API endpoints
#[derive(Clone, ModuleInfo, ModuleRestApi)]
pub struct ValueSetterZk<S: Spec> {
    /// The ID of the module.
    #[id]
    pub id: ModuleId,

    /// The stored value. Can only be updated with a valid proof.
    #[state]
    pub value: StateValue<u32>,

    /// Code commitment (32 bytes) of the Ligetron guest program that verifies value constraints.
    /// This is the SHA-256 hash of (WASM program bytes || packing parameter).
    #[state]
    pub method_id: StateValue<[u8; 32]>,

    /// Administrator address who can update the method ID.
    #[state]
    pub admin: StateValue<S::Address>,
}

impl<S: Spec> Module for ValueSetterZk<S> {
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
            CallMessage::SetValueWithProof { value, proof, gas } => {
                Ok(self.set_value_with_proof(value, proof, gas, context, state)?)
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

