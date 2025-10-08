use std::fmt::Debug;

use anyhow::{anyhow, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::{Context, EventEmitter, Gas, Spec, TxState};
use sov_rollup_interface::zk::CodeCommitment;
use thiserror::Error;

use super::ValueSetterZk;
use crate::event::Event;

/// Public output from the Ligetron guest program.
/// This structure is committed in the proof's journal and verified on-chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueProofPublic {
    /// The value that was proven to be valid
    pub value: u32,
}

/// Available call messages for the `ValueSetterZk` module.
#[derive(Debug, PartialEq, Eq, Clone, JsonSchema, UniversalWallet)]
#[serialize(Borsh, Serde)]
#[schemars(bound = "S::Gas: ::schemars::JsonSchema", rename = "CallMessage")]
#[serde(rename_all = "snake_case")]
pub enum CallMessage<S: Spec> {
    /// Set a new value with ZK proof verification.
    /// The proof must demonstrate that the value is within the valid range [0, 65535].
    SetValueWithProof {
        /// The value to set
        value: u32,
        /// Serialized Ligetron proof package (bincode-encoded)
        /// Note: Ligero proofs are typically 2-4MB in size
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        /// Gas to charge. Don't charge gas if None.
        gas: Option<S::Gas>,
    },
    
    /// Update the method ID (admin only).
    /// This allows upgrading the guest program used for proof verification.
    UpdateMethodId {
        /// The new method ID (32-byte SHA-256 hash)
        new_method_id: [u8; 32],
    },
}

/// Errors that can occur when setting a value with proof.
#[derive(Debug, Error)]
pub enum SetValueZkError<S: Spec> {
    /// Value tried to be set by a non-admin when updating method ID.
    #[error(
        "Only admin can update the method ID. The expected admin is {admin}, but the sender is {sender}"
    )]
    WrongSender {
        /// The expected admin.
        admin: S::Address,
        /// The sender.
        sender: S::Address,
    },
    
    /// The proof verification failed.
    #[error("Proof verification failed: {0}")]
    ProofVerificationFailed(String),
    
    /// The value in the proof doesn't match the requested value.
    #[error("Proof mismatch: journal value {journal_value} != requested value {requested_value}")]
    ValueMismatch {
        /// The value committed in the proof
        journal_value: u32,
        /// The value requested to be set
        requested_value: u32,
    },
}

impl<S: Spec> ValueSetterZk<S> {
    /// Set `value` to `new_value` if the provided `proof` verifies that the value is valid.
    ///
    /// The proof must:
    /// 1. Be verifiable against the configured method ID
    /// 2. Commit to a `ValueProofPublic` struct in its journal
    /// 3. Have a journal value matching the requested value
    ///
    /// The guest program enforces that:
    /// - The value is within [0, 65535]
    /// - The proven value matches the claimed value (prevents proof substitution attacks)
    pub(crate) fn set_value_with_proof(
        &mut self,
        value: u32,
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        gas: Option<S::Gas>,
        _context: &Context<S>,
        state: &mut impl TxState<S>,
    ) -> Result<()> {
        // Charge gas first
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        state.charge_gas(&gas)?;
        
        // Get the configured method ID from state
        let method_id_bytes = self
            .method_id
            .get(state)?
            .ok_or_else(|| anyhow!("method_id not configured in module state"))?;
        
        // Verify the proof using LigeroVerifier
        #[cfg(feature = "native")]
        {
            use sov_ligero_adapter::{LigeroCodeCommitment, LigeroVerifier, LigeroProofPackage};
            
            let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;
            
            // Deserialize the proof package
            let package: LigeroProofPackage<ValueProofPublic> = bincode::deserialize(&proof)
                .map_err(|e| anyhow!("Failed to deserialize proof package: {}", e))?;
            
            // SECURITY CRITICAL: Verify the proof with BOTH the proven value and claimed value
            // The WASM program will assert that proven_value == claimed_value
            // This prevents proof substitution attacks where attacker uses proof for value X to claim value Y
            let public: ValueProofPublic = LigeroVerifier::verify_with_value(&package.proof, &method_id, value)
                .map_err(|e| SetValueZkError::<S>::ProofVerificationFailed(e.to_string()))?;
            
            // Double-check: Ensure the verified journal matches the requested value
            // This is redundant with WASM check but provides defense in depth
            if public.value != value {
                return Err(SetValueZkError::<S>::ValueMismatch {
                    journal_value: public.value,
                    requested_value: value,
                }.into());
            }
        }
        
        // In non-native mode (e.g., inside a zkVM), we can't verify Ligero proofs
        #[cfg(not(feature = "native"))]
        {
            anyhow::bail!(
                "Ligero proof verification is only supported in native mode. \
                 The value-setter-zk module cannot be used inside a zkVM."
            );
        }
        
        // All checks passed - set the value
        self.value.set(&value, state)?;
        
        // Emit event
        self.emit_event(state, Event::ValueSetWithProof { value });
        
        Ok(())
    }
    
    /// Update the method ID (admin only).
    pub(crate) fn update_method_id(
        &mut self,
        new_method_id: [u8; 32],
        context: &Context<S>,
        state: &mut impl TxState<S>,
    ) -> Result<()> {
        // Check admin authorization
        let admin = self.admin.get_or_err(state)??;
        
        if &admin != context.sender() {
            return Err(SetValueZkError::WrongSender::<S> {
                admin,
                sender: context.sender().clone(),
            }.into());
        }
        
        // Update the method ID
        self.method_id.set(&new_method_id, state)?;
        
        // Emit event
        self.emit_event(state, Event::MethodIdUpdated { new_method_id });
        
        Ok(())
    }
}

