use std::fmt::Debug;

use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::{Context, EventEmitter, Gas, Spec, TxState};
use thiserror::Error;

use super::ValueMidnightPrivacy;
use crate::event::Event;
use crate::hash::{note_commitment, Hash32};
use crate::types::Note;

#[cfg(feature = "native")]
use anyhow::anyhow;

#[cfg(feature = "native")]
use crate::hash::NullifierKey;

#[cfg(feature = "native")]
use crate::types::SpendPublic;

/// Available call messages for the `MidnightPrivacy` module.
#[derive(Debug, PartialEq, Eq, Clone, JsonSchema, UniversalWallet)]
#[serialize(Borsh, Serde)]
#[schemars(bound = "S::Gas: ::schemars::JsonSchema", rename = "CallMessage")]
#[serde(rename_all = "snake_case")]
pub enum CallMessage<S: Spec> {
    /// Create a new note commitment and add it to the tree.
    CreateNote {
        /// The note to create
        note: Note,
        /// Gas to charge. Don't charge gas if None.
        gas: Option<S::Gas>,
    },
    
    /// Spend a note by providing a ZK proof and consuming its nullifier.
    /// The proof must demonstrate:
    /// 1. Knowledge of a note commitment in the tree
    /// 2. A valid Merkle path from the note to an anchor root
    /// 3. Proper nullifier derivation
    SpendNote {
        /// Serialized Ligero proof package (bincode-encoded)
        /// Note: Ligero proofs are typically 2-4MB in size
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        /// Gas to charge. Don't charge gas if None.
        gas: Option<S::Gas>,
    },

    /// Deposit tokens into the pool and create a note commitment.
    /// Moves `amount` of the native token from sender into the pool.
    Deposit {
        /// Amount to deposit
        amount: u128,
        /// Random nonce for the note
        rho: Hash32,
        /// Recipient binding
        recipient: Hash32,
        /// Gas to charge. Don't charge gas if None.
        gas: Option<S::Gas>,
    },

    /// Withdraw tokens from the pool after verifying a ZK proof.
    /// The proof must bind the withdrawal amount to prevent draining attacks.
    Withdraw {
        /// Serialized Ligero proof package (bincode-encoded)
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        /// Recipient address for the withdrawn tokens
        to: S::Address,
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

/// Errors that can occur in the MidnightPrivacy module.
#[derive(Debug, Error)]
pub enum MidnightPrivacyError<S: Spec> {
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
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    ProofVerificationFailed(String),
    
    /// The nullifier has already been used.
    #[error("Nullifier already spent: {}", hex::encode(.0))]
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    NullifierAlreadySpent(Hash32),
    
    /// The anchor root is not in the recent roots window.
    #[error("Invalid anchor root: {}", hex::encode(.0))]
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    InvalidAnchorRoot(Hash32),
    
    /// The tree is full.
    #[error("Commitment tree is full (max size: {0})")]
    TreeFull(usize),

    /// Amount conversion overflow.
    #[error("Amount {0} does not fit in the bank amount type")]
    AmountOverflow(u128),
}

impl<S: Spec> ValueMidnightPrivacy<S> {
    /// Create a new note commitment and add it to the Merkle tree.
    pub(crate) fn create_note(
        &mut self,
        mut note: Note,
        gas: Option<S::Gas>,
        _context: &Context<S>,
        state: &mut impl TxState<S>,
    ) -> Result<()> {
        // Charge gas first
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        state.charge_gas(&gas)?;

        // Force the note domain to match configured domain (defense-in-depth)
        note.domain = self.domain.get_or_err(state)??;
        
        // Get current tree and position
        let mut tree = self.commitment_tree.get_or_err(state)??;
        let next_position = self.next_position.get_or_err(state)??;
        
        // Check if tree is full
        if next_position >= tree.len() as u64 {
            return Err(MidnightPrivacyError::<S>::TreeFull(tree.len()).into());
        }
        
        // Compute note commitment
        let cm = note_commitment(&note.domain, note.value, &note.rho, &note.recipient);
        
        // Add to tree
        tree.set_leaf(next_position as usize, cm);
        let new_root = tree.root();
        
        // Update state
        self.commitment_tree.set(&tree, state)?;
        self.next_position.set(&(next_position + 1), state)?;
        
        // Add root to recent roots window
        self.add_recent_root(new_root, state)?;
        
        // Emit event
        self.emit_event(state, Event::NoteCreated {
            commitment: cm,
            position: next_position,
            new_root,
        });
        
        Ok(())
    }

    /// Deposit: transfer tokens into the pool, append commitment, update root window.
    pub(crate) fn deposit(
        &mut self,
        amount: u128,
        rho: Hash32,
        recipient: Hash32,
        gas: Option<S::Gas>,
        ctx: &Context<S>,
        st: &mut impl TxState<S>,
    ) -> Result<()> {
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        st.charge_gas(&gas)?;

        // Convert amount into the bank's amount type (u64 -> Amount)
        let amount_u64: u64 = amount
            .try_into()
            .map_err(|_| MidnightPrivacyError::<S>::AmountOverflow(amount))?;
        let bank_amount = sov_bank::Amount::from(amount_u64);

        // Pull native token from sender into module account
        use sov_bank::IntoPayable;
        let token_id = self.token_id.get_or_err(st)??;
        let coins = sov_bank::Coins {
            amount: bank_amount,
            token_id,
        };
        self.bank.transfer_from(
            ctx.sender(),
            self.id.to_payable(),
            coins,
            st,
        )?;

        // Build note using the configured domain
        let domain = self.domain.get_or_err(st)??;
        let note = Note {
            domain,
            value: amount,
            rho,
            recipient,
        };

        // Reuse create_note to append to the tree + emit events
        self.create_note(note, Some(gas), ctx, st)?;

        // Emit explicit pool deposit event
        let cm = note_commitment(&domain, amount, &rho, &recipient);
        let next_pos = self.next_position.get_or_err(st)??;
        let position = next_pos.saturating_sub(1);
        let tree = self.commitment_tree.get_or_err(st)??;
        let new_root = tree.root();

        self.emit_event(
            st,
            Event::PoolDeposit {
                amount,
                commitment: cm,
                position,
                new_root,
            },
        );

        Ok(())
    }

    /// Withdraw: verify proof, consume nullifier, and transfer native token out.
    pub(crate) fn withdraw(
        &mut self,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] to: S::Address,
        gas: Option<S::Gas>,
        _ctx: &Context<S>,
        st: &mut impl TxState<S>,
    ) -> Result<()> {
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        st.charge_gas(&gas)?;

        // Verify proof and extract public output
        #[cfg(not(feature = "native"))]
        {
            anyhow::bail!("Ligero verification requires the \"native\" feature enabled");
        }

        #[cfg(feature = "native")]
        {
            use sov_ligero_adapter::{LigeroCodeCommitment, LigeroVerifier};
            use sov_rollup_interface::zk::{CodeCommitment, ZkVerifier};
            
            let method_id_bytes = self
                .method_id
                .get(st)?
                .ok_or_else(|| anyhow!("method_id not configured in module state"))?;
            
            let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;
            
            let public: SpendPublic = LigeroVerifier::verify(&proof, &method_id)
                .map_err(|e| MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string()))?;

            // 1) Anchor must be valid
            if !self.is_valid_anchor(&public.anchor_root, st)? {
                return Err(MidnightPrivacyError::<S>::InvalidAnchorRoot(public.anchor_root).into());
            }

            // 2) Nullifier must be fresh
            let nk = NullifierKey(public.nullifier);
            if self.nullifier_set.get(&nk, st)?.is_some() {
                return Err(MidnightPrivacyError::<S>::NullifierAlreadySpent(public.nullifier).into());
            }
            self.nullifier_set.set(&nk, &true, st)?;

            // 3) Perform the transparent withdrawal for the *authorized* amount
            use sov_bank::IntoPayable;
            let token_id = self.token_id.get_or_err(st)??;

            let amount_u64: u64 = public
                .withdraw_amount
                .try_into()
                .map_err(|_| MidnightPrivacyError::<S>::AmountOverflow(public.withdraw_amount))?;
            let bank_amount = sov_bank::Amount::from(amount_u64);

            let coins = sov_bank::Coins {
                amount: bank_amount,
                token_id,
            };
            self.bank.transfer_from(
                self.id.to_payable(),
                &to,
                coins,
                st,
            )?;

            // Emit events
            self.emit_event(
                st,
                Event::NoteSpent {
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                },
            );
            self.emit_event(
                st,
                Event::PoolWithdraw {
                    amount: public.withdraw_amount,
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                },
            );

            Ok(())
        }
    }
    
    /// Spend a note by verifying a ZK proof and consuming its nullifier.
    ///
    /// The proof must:
    /// 1. Be verifiable against the configured method ID
    /// 2. Commit to a `SpendPublic` struct containing (anchor_root, nullifier, withdraw_amount)
    /// 3. The anchor_root must be in the recent roots window
    /// 4. The nullifier must not have been seen before
    /// 5. For pure spends with no withdrawal, withdraw_amount should be 0
    pub(crate) fn spend_note(
        &mut self,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        gas: Option<S::Gas>,
        _context: &Context<S>,
        state: &mut impl TxState<S>,
    ) -> Result<()> {
        // Charge gas first
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        state.charge_gas(&gas)?;
        
        // In non-native mode (e.g., inside a zkVM), we can't verify Ligero proofs
        #[cfg(not(feature = "native"))]
        {
            anyhow::bail!(
                "Ligero proof verification is only supported in native mode. \
                 The midnight-privacy module cannot be used inside a zkVM."
            );
        }
        
        // Verify the proof using LigeroVerifier
        #[cfg(feature = "native")]
        {
            use sov_ligero_adapter::{LigeroCodeCommitment, LigeroVerifier};
            use sov_rollup_interface::zk::{CodeCommitment, ZkVerifier};
            
            // Get the configured method ID from state
            let method_id_bytes = self
                .method_id
                .get(state)?
                .ok_or_else(|| anyhow!("method_id not configured in module state"))?;
            
            let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;
            
            let public: SpendPublic = LigeroVerifier::verify(&proof, &method_id)
                .map_err(|e| MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string()))?;
            
            // Check that the anchor root is valid (in recent roots window)
            if !self.is_valid_anchor(&public.anchor_root, state)? {
                return Err(MidnightPrivacyError::<S>::InvalidAnchorRoot(public.anchor_root).into());
            }
            
            // Check that the nullifier hasn't been used
            let nullifier_key = NullifierKey(public.nullifier);
            if self.nullifier_set.get(&nullifier_key, state)?.is_some() {
                return Err(MidnightPrivacyError::<S>::NullifierAlreadySpent(public.nullifier).into());
            }
            
            // Mark the nullifier as used
            self.nullifier_set.set(&nullifier_key, &true, state)?;
            
            // Emit event
            self.emit_event(state, Event::NoteSpent {
                nullifier: public.nullifier,
                anchor_root: public.anchor_root,
            });
            
            // Note: For pure spends without withdrawal, withdraw_amount should be 0.
            // If withdraw_amount > 0, this indicates the proof authorizes a withdrawal
            // but the caller should use the Withdraw call instead.
            
            Ok(())
        }
    }
    
    /// Add a root to the recent roots window (circular buffer).
    fn add_recent_root(&mut self, root: Hash32, state: &mut impl TxState<S>) -> Result<()> {
        let mut recent_roots = self.recent_roots.get_or_err(state)??;
        let root_window_size = self.root_window_size.get_or_err(state)??;
        
        // Add to end, remove from front if full
        recent_roots.push(root);
        if recent_roots.len() > root_window_size as usize {
            recent_roots.remove(0);
        }
        
        self.recent_roots.set::<Vec<Hash32>, _>(&recent_roots, state)?;
        Ok(())
    }
    
    /// Check if an anchor root is valid (in the recent roots window).
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    fn is_valid_anchor(&self, anchor: &Hash32, state: &mut impl TxState<S>) -> Result<bool> {
        let recent_roots = self.recent_roots.get_or_err(state)??;
        Ok(recent_roots.contains(anchor))
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
            return Err(MidnightPrivacyError::WrongSender::<S> {
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

