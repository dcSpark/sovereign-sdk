use std::fmt::Debug;

use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::{Context, EventEmitter, Gas, Spec, TxState};
use thiserror::Error;

use super::ValueMidnightPrivacy;
use crate::event::Event;
use crate::hash::{note_commitment, Hash32, RootKey};
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
    /// 
    /// SECURITY: anchor_root, nullifier, and withdraw_amount are passed as explicit
    /// transaction fields (NOT extracted from public_output) and are validated by the guest.
    /// This prevents public-output tampering attacks.
    Withdraw {
        /// Serialized Ligero proof package (bincode-encoded)
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        /// Anchor root that the proof is bound to (must be in recent roots window)
        anchor_root: Hash32,
        /// Nullifier that the proof derives (must be fresh)
        nullifier: Hash32,
        /// Withdrawal amount authorized by the proof
        withdraw_amount: u128,
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
    
    /// Proof outputs do not match transaction arguments.
    #[error("Proof outputs do not match transaction arguments")]
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    PublicOutputMismatch,
    
    /// Value-burning spend attempt (nullifier-only with no outputs or withdrawal).
    #[error("Cannot consume nullifier without value movement: must either withdraw transparently or create output notes")]
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    ValueBurningSpend,
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
        
        // 1) Add root to recent roots window (fast mempool checks)
        self.add_recent_root(new_root, state)?;
        // 2) Permanently record in NOMT-backed index (full history for long-range anchors)
        self.record_root_forever(new_root, state)?;
        
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
    /// 
    /// SECURITY NOTE (Option A - explicit transaction fields):
    /// The anchor_root, nullifier, and withdraw_amount are taken from TRANSACTION ARGUMENTS,
    /// not from the proof's public_output. The guest program verifies these values match its
    /// computations. This prevents "unbound journal" attacks where an attacker could tamper
    /// with public_output while keeping a valid proof.
    pub(crate) fn withdraw(
        &mut self,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] anchor_root: Hash32,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] nullifier: Hash32,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] withdraw_amount: u128,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] to: S::Address,
        gas: Option<S::Gas>,
        _ctx: &Context<S>,
        st: &mut impl TxState<S>,
    ) -> Result<()> {
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        st.charge_gas(&gas)?;

        // Verify proof and validate explicit transaction fields
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
            
            // Verify the proof and extract the proof-committed public output
            let public: SpendPublic = LigeroVerifier::verify(&proof, &method_id)
                .map_err(|e| MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string()))?;
            
            // CRITICAL SECURITY: Bind transaction fields to proof-committed values
            // The proof commits to specific (anchor_root, nullifier, withdraw_amount).
            // We must verify the transaction fields match what the proof committed to,
            // otherwise an attacker could provide a valid proof for (A, B, C) but
            // submit transaction fields (X, Y, Z) and we'd accept them.
            if public.anchor_root != anchor_root
                || public.nullifier != nullifier
                || public.withdraw_amount != withdraw_amount
            {
                return Err(MidnightPrivacyError::<S>::PublicOutputMismatch.into());
            }

            // Now use the PROOF-COMMITTED values (which we've verified match the tx fields)
            // for all state changes. This ensures cryptographic binding.

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

            // 3) Transfer authorized amount
            use sov_bank::IntoPayable;
            let token_id = self.token_id.get_or_err(st)??;

            let amount_u64: u64 = public.withdraw_amount
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

            // Emit events (using proof-committed values)
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
    ///
    /// **VALUE PRESERVATION**: To prevent accidental value burning, this call is currently
    /// rejected. Use `Withdraw` to move value to a transparent address.
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
            
            // CRITICAL: Prevent value-burning spends
            // In a value-preserving shielded pool (like Zcash), consuming a nullifier must
            // move value somewhere: either to new shielded notes or to transparent withdrawal.
            // Since this call doesn't track output notes and has no withdrawal, it would burn value.
            // Reject it to prevent accidental burns. Use `Withdraw` for transparent value movement.
            if public.withdraw_amount == 0 {
                return Err(MidnightPrivacyError::<S>::ValueBurningSpend.into());
            }
            
            // If withdraw_amount > 0, the caller should use the Withdraw call instead
            // which properly handles the withdrawal and binding
            return Err(anyhow!(
                "SpendNote with withdraw_amount > 0 should use Withdraw call instead"
            ).into());
        }
    }
    
    /// Add a root to the recent roots window (circular buffer).
    /// Uses VecDeque for O(1) operations at both ends.
    /// This provides fast mempool checks for recent transactions.
    fn add_recent_root(&mut self, root: Hash32, state: &mut impl TxState<S>) -> Result<()> {
        let mut recent_roots = self.recent_roots.get_or_err(state)??;
        let root_window_size = self.root_window_size.get_or_err(state)??;
        
        // Add to end, remove from front if full (O(1) with VecDeque)
        recent_roots.push_back(root);
        if recent_roots.len() > root_window_size as usize {
            recent_roots.pop_front();
        }
        
        self.recent_roots.set(&recent_roots, state)?;
        Ok(())
    }
    
    /// Permanently record a root in the full-history NOMT-backed index.
    /// This enables long-range anchor validation: any historical root remains valid forever.
    /// Idempotent: if the root already exists, this is a no-op.
    fn record_root_forever(&mut self, root: Hash32, state: &mut impl TxState<S>) -> Result<()> {
        // Fast path: already recorded?
        if self.all_roots.get(&RootKey(root), state)?.is_some() {
            return Ok(());
        }
        
        // Assign a monotonic sequence number and commit to permanent storage
        let seq = self.root_seq.get_or_err(state)??;
        self.all_roots.set(&RootKey(root), &seq, state)?;
        
        // Emit event for observability
        self.emit_event(state, Event::AnchorRootRecorded {
            root,
            seq,
        });
        
        // Bump sequence (checked add to be safe against overflow)
        let next_seq = seq.checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("root_seq overflow: too many unique roots"))?;
        self.root_seq.set(&next_seq, state)?;
        
        Ok(())
    }
    
    /// Check if an anchor root is valid:
    ///  - first, in the recent roots window (cheap O(n) scan of VecDeque);
    ///  - else, in the permanent NOMT-backed index (`all_roots`, O(log N) lookup).
    /// 
    /// This enables long-range anchors: any historical root remains valid forever,
    /// aligning with Zcash's design (ZIP-221) where roots are permanently accessible.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    fn is_valid_anchor(&self, anchor: &Hash32, state: &mut impl TxState<S>) -> Result<bool> {
        let recent_roots = self.recent_roots.get_or_err(state)??;
        
        // Fast path: check recent window first (common case for active transactions)
        if recent_roots.contains(anchor) {
            return Ok(true);
        }
        
        // Fallback: check permanent historical index (enables long-range proofs)
        Ok(self.all_roots.get(&RootKey(*anchor), state)?.is_some())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that RootKey serialization works correctly
    #[test]
    fn test_root_key_display_and_parse() {
        let root = [42u8; 32];
        let root_key = RootKey(root);
        
        // Test Display
        let display_str = format!("{}", root_key);
        assert_eq!(display_str, hex::encode(root));
        
        // Test FromStr
        let parsed: RootKey = display_str.parse().unwrap();
        assert_eq!(parsed, root_key);
    }
}

