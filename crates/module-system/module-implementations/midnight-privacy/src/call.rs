use std::fmt::Debug;

use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::VersionReader;
use sov_modules_api::{Context, EventEmitter, Gas, Spec, TxState};
use std::collections::HashSet;
use thiserror::Error;
use tracing::{debug, info};

use super::ValueMidnightPrivacy;
use crate::event::Event;
use crate::hash::{note_commitment, Hash32, PendingRootKey, RootKey};
use crate::types::{EncryptedNote, FullViewingKey};

#[cfg(feature = "native")]
use anyhow::anyhow;

#[cfg(feature = "native")]
use crate::hash::NullifierKey;

/// Available call messages for the `MidnightPrivacy` module.
#[derive(Debug, PartialEq, Eq, Clone, JsonSchema, UniversalWallet)]
#[serialize(Borsh, Serde)]
#[schemars(bound = "S::Gas: ::schemars::JsonSchema", rename = "CallMessage")]
#[serde(rename_all = "snake_case")]
pub enum CallMessage<S: Spec> {
    /// Deposit: Put money INTO the privacy pool (Transparent → Shielded).
    ///
    /// Moves `amount` of the native token from sender into the shielded pool,
    /// creating a single note commitment.
    Deposit {
        /// Amount to deposit
        amount: u128,
        /// Random nonce for the note
        rho: Hash32,
        /// Recipient binding
        recipient: Hash32,
        /// Optional list of Full Viewing Keys to emit encrypted payloads for this deposit note.
        /// For each FVK, the module will encrypt the note and emit `NoteEncrypted`.
        view_fvks: Option<Vec<FullViewingKey>>,
        /// Gas to charge. Don't charge gas if None.
        gas: Option<S::Gas>,
    },

    /// Transfer: Move money WITHIN the privacy pool (Shielded → Shielded).
    ///
    /// This is the pure privacy-preserving transaction that atomically:
    /// 1. Verifies a ZK proof
    /// 2. Consumes the input note's nullifier (prevents double-spending)
    /// 3. Creates output note commitments from the proof
    ///
    /// The proof demonstrates:
    /// - Knowledge of a note in the tree (via Merkle path)
    /// - Proper nullifier derivation
    /// - Value conservation: input_value = sum(output_values)
    ///
    /// SECURITY: anchor_root and nullifier are passed as explicit transaction fields
    /// (NOT extracted from public_output) and are validated by the guest.
    /// This prevents public-output tampering attacks.
    ///
    /// # Examples
    ///
    /// - Split 1000 → 600 + 400 (two outputs)
    /// - Consolidate multiple notes into one
    /// - Send shielded payment to another recipient
    Transfer {
        /// Serialized Ligero proof package (bincode-encoded)
        /// Note: Ligero proofs are typically 2-4MB in size
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        /// Anchor root that the proof is bound to (must be valid historical root)
        anchor_root: Hash32,
        /// Nullifier that the proof derives (must be fresh)
        nullifier: Hash32,
        /// Optional viewer ciphertexts (created off-chain by the prover).
        /// Each EncryptedNote must have `enc.cm` equal to one of the produced output commitments.
        view_ciphertexts: Option<Vec<EncryptedNote>>,
        /// Gas to charge. Don't charge gas if None.
        gas: Option<S::Gas>,
    },

    /// Withdraw: Take money OUT of the privacy pool (Shielded → Transparent).
    ///
    /// This call atomically:
    /// 1. Verifies a ZK proof
    /// 2. Consumes the input note's nullifier (prevents double-spending)
    /// 3. Creates output note commitments from the proof (for change/split)
    /// 4. Transfers transparent tokens to the recipient
    ///
    /// The proof demonstrates:
    /// - Knowledge of a note in the tree
    /// - Proper nullifier derivation
    /// - Value conservation: input_value = sum(output_values) + withdraw_amount
    ///
    /// SECURITY: anchor_root, nullifier, and withdraw_amount are explicit transaction
    /// fields validated by the guest to prevent tampering.
    ///
    /// # Examples
    ///
    /// - Full withdrawal: Input 1000 → Withdraw 1000 (no outputs)
    /// - Partial withdrawal: Input 1000 → Withdraw 600 + Change 400 (one output)
    Withdraw {
        /// Serialized Ligero proof package (bincode-encoded)
        /// Note: Ligero proofs are typically 2-4MB in size
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        /// Anchor root that the proof is bound to (must be valid historical root)
        anchor_root: Hash32,
        /// Nullifier that the proof derives (must be fresh)
        nullifier: Hash32,
        /// Withdrawal amount authorized by the proof
        withdraw_amount: u128,
        /// Recipient address for the withdrawn tokens
        to: S::Address,
        /// Optional viewer ciphertexts (created off-chain by the prover).
        /// Each EncryptedNote must have `enc.cm` equal to one of the produced *change* outputs.
        view_ciphertexts: Option<Vec<EncryptedNote>>,
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

    /// Amount conversion overflow.
    #[error("Amount {0} does not fit in the bank amount type")]
    AmountOverflow(u128),

    /// Proof outputs do not match transaction arguments.
    #[error("Proof outputs do not match transaction arguments")]
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    PublicOutputMismatch,

    /// Number of output commitments in proof exceeds maximum allowed
    #[error("Too many output commitments: {0} (max: {1})")]
    TooManyOutputs(usize, usize),
}

impl<S: Spec> ValueMidnightPrivacy<S> {
    /// Internal helper: Queue a single commitment for end-of-block processing.
    /// Used by deposit() and transfer() to append note commitments.
    ///
    /// BLOCK-DEFERRED DESIGN: The commitment_tree is NOT updated here. Instead:
    /// 1. Commitment is queued for this block (per tx, unique key)
    /// 2. Positions and roots are assigned in end_block_flush (once per block)
    ///
    /// This reduces per-tx state writes from ~4MB (full tree) to ~100 bytes,
    /// dramatically reducing cache memory usage.

    /// Queue a commitment for end-of-block processing.
    ///
    /// This function does NOT update the tree or assign positions. It only enqueues
    /// the commitment. All tree updates and position assignments happen in `end_block_flush`,
    /// which runs single-threaded after all transactions complete.
    ///
    /// PARALLEL-SAFE: Each commitment writes to a unique key `(height, commitment)`,
    /// so there are no write conflicts between concurrent transactions.
    /// Enumeration at flush time uses StateMap::iter_prefix to find all commitments
    /// for the current height.
    fn add_commitment(&mut self, commitment: Hash32, state: &mut impl TxState<S>) -> Result<()> {
        let current_height = state.rollup_height_to_access();

        // Store commitment with unique key (height, commitment) - no conflicts in parallel execution
        // Value is just a presence marker - position is assigned at flush time.
        let cm_key = crate::hash::PendingCommitmentKey {
            height: current_height.get(),
            commitment,
        };
        self.pending_commitments_by_hash.set(&cm_key, &(), state)?;

        // Emit event - position is assigned at flush time
        self.emit_event(state, Event::NoteCreated { commitment });

        Ok(())
    }

    /// Queue a nullifier for end-of-block processing.
    ///
    /// This function does NOT update the nullifier tree. It only enqueues the nullifier.
    /// Tree updates happen in `end_block_flush`.
    ///
    /// PARALLEL-SAFE: Each nullifier writes to a unique key `(height, nullifier)`,
    /// so there are no write conflicts between concurrent transactions.
    ///
    /// Note: Double-spend protection is done via `nullifier_set` (O(1) lookup, per tx).
    /// The nullifier tree is for canonical root tracking and future circuit integration.
    fn append_nullifier(&mut self, nullifier: Hash32, state: &mut impl TxState<S>) -> Result<()> {
        let current_height = state.rollup_height_to_access();

        // Store nullifier with unique key (height, nullifier) - no conflicts
        let nf_key = crate::hash::PendingNullifierKey {
            height: current_height.get(),
            nullifier,
        };
        self.pending_nullifiers_by_hash.set(&nf_key, &(), state)?;

        Ok(())
    }

    /// Deposit: transfer tokens into the pool, append commitment, update root window.
    pub(crate) fn deposit(
        &mut self,
        amount: u128,
        rho: Hash32,
        recipient: Hash32,
        view_fvks: Option<Vec<FullViewingKey>>,
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
        self.bank
            .transfer_from(ctx.sender(), self.id.to_payable(), coins, st)?;

        // Compute commitment and queue for end-of-block processing
        let domain = self.domain.get_or_err(st)??;
        let cm = note_commitment(&domain, amount, &rho, &recipient);
        self.add_commitment(cm, st)?;

        // Optional: emit viewer ciphertexts for the deposit note
        if let Some(fvks) = view_fvks {
            // Cap to avoid event spam
            const MAX_VIEW_CT: usize = 8;
            let fvks = fvks.into_iter().take(MAX_VIEW_CT);
            let note = crate::types::Note {
                domain,
                value: amount,
                rho,
                recipient,
            };
            for fvk in fvks {
                let enc = crate::viewing::encrypt_note_for_fvk(&fvk, &note, &cm)
                    .map_err(|e| anyhow::anyhow!("viewer encrypt (deposit): {e}"))?;
                self.emit_event(st, Event::NoteEncrypted { enc });
            }
        }

        // Update deposit statistics
        let total_deposited = self.total_deposited.get(st)?.unwrap_or(0);
        self.total_deposited.set(&(total_deposited + amount), st)?;

        let deposit_count = self.deposit_count.get(st)?.unwrap_or(0);
        self.deposit_count.set(&(deposit_count + 1), st)?;

        // Emit pool deposit event (position is provisional, assigned at flush)
        self.emit_event(
            st,
            Event::PoolDeposit {
                amount,
                commitment: cm,
            },
        );

        Ok(())
    }

    /// Transfer: Move money WITHIN the privacy pool (pure shielded transaction).
    ///
    /// This method atomically:
    /// 1. Verifies a ZK proof
    /// 2. Consumes the input note's nullifier (prevents double-spending)
    /// 3. Creates output note commitments from the proof
    ///
    /// All value stays shielded. For transparent withdrawals, use `withdraw()`.
    ///
    /// SECURITY: anchor_root and nullifier are passed as explicit transaction fields
    /// and validated against the proof to prevent tampering.
    pub(crate) fn transfer(
        &mut self,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))]
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] anchor_root: Hash32,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] nullifier: Hash32,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] view_ciphertexts: Option<
            Vec<EncryptedNote>,
        >,
        gas: Option<S::Gas>,
        _ctx: &Context<S>,
        st: &mut impl TxState<S>,
    ) -> Result<()> {
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        st.charge_gas(&gas)?;

        #[cfg(not(feature = "native"))]
        {
            anyhow::bail!("Ligero verification requires the \"native\" feature enabled");
        }

        #[cfg(feature = "native")]
        {
            use sov_ligero_adapter::{LigeroCodeCommitment, LigeroVerifier};
            use sov_rollup_interface::zk::{CodeCommitment, ZkVerifier};

            let credential_check_start = std::time::Instant::now();
            let cached_public = crate::get_pre_verified_spend(&nullifier);
            let credential_check_duration = credential_check_start.elapsed();
            debug!(
                credential_check_ms = ?(credential_check_duration.as_secs_f64() * 1000.0),
                has_pre_verified = cached_public.is_some(),
                "Transfer: checked for pre-verified proof outputs"
            );

            let public = if let Some(public) = cached_public {
                debug!("Using pre-verified path (skipping Ligero proof verification)");
                public
            } else {
                info!("No pre-verified credential, performing full Ligero proof verification");

                // Only load and decode method_id when we really need to verify a proof.
                let method_id_bytes = self
                    .method_id
                    .get(st)?
                    .ok_or_else(|| anyhow!("method_id not configured in module state"))?;

                let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                    .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;

                // Verify the proof and extract public output
                LigeroVerifier::verify(&proof, &method_id).map_err(|e| {
                    MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string())
                })?
            };

            // SECURITY: Bind transaction fields to proof-committed values
            if public.anchor_root != anchor_root || public.nullifier != nullifier {
                return Err(MidnightPrivacyError::<S>::PublicOutputMismatch.into());
            }

            // Ensure this is a pure shielded transfer (no withdrawal)
            if public.withdraw_amount != 0 {
                return Err(anyhow!(
                    "Transfer must have withdraw_amount = 0. Use Withdraw call for transparent outputs."
                )
                .into());
            }

            // Limit outputs to at most 2 (per requirements)
            const MAX_OUTPUTS_TRANSFER: usize = 2;
            if public.output_commitments.len() > MAX_OUTPUTS_TRANSFER {
                return Err(MidnightPrivacyError::<S>::TooManyOutputs(
                    public.output_commitments.len(),
                    MAX_OUTPUTS_TRANSFER,
                )
                .into());
            }

            // 1) Validate anchor root
            if !self.is_valid_anchor(&public.anchor_root, st)? {
                return Err(
                    MidnightPrivacyError::<S>::InvalidAnchorRoot(public.anchor_root).into(),
                );
            }

            // 2) Consume nullifier
            let nk = NullifierKey(public.nullifier);
            if self.nullifier_set.get(&nk, st)?.is_some() {
                return Err(
                    MidnightPrivacyError::<S>::NullifierAlreadySpent(public.nullifier).into(),
                );
            }
            self.nullifier_set.set(&nk, &true, st)?;
            // Stats: bump spent nullifier count
            let n_spent = self.spent_nullifier_count.get(st)?.unwrap_or(0);
            self.spent_nullifier_count.set(&(n_spent + 1), st)?;

            // Record nullifier in the nullifier tree (Aztec-style dual-tree design)
            self.append_nullifier(public.nullifier, st)?;

            // 3) Queue all output commitments for end-of-block processing
            let outputs: Vec<Hash32> = public.output_commitments.clone();
            for cm in &outputs {
                self.add_commitment(*cm, st)?;
            }

            // Emit spent event
            self.emit_event(
                st,
                Event::NoteSpent {
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                },
            );

            // Level B: Viewer attestation verification
            if let Some(vcs) = view_ciphertexts {
                use crate::viewing::ct_hash as compute_ct_hash;

                const MAX_VIEW_CT: usize = 16; // reasonable upper bound for viewer ciphertexts
                let outputs_set: HashSet<Hash32> = outputs.iter().copied().collect();

                // Require Level B attestations when ciphertexts are present
                let attestations = public.view_attestations.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Level B required: proof must include view_attestations when view_ciphertexts are present"
                    )
                })?;

                // Build attestation lookup: (cm, fvk_commitment) -> (ct_hash, mac)
                let mut att_map: std::collections::HashMap<(Hash32, Hash32), (Hash32, Hash32)> =
                    std::collections::HashMap::new();
                for att in attestations {
                    att_map.insert((att.cm, att.fvk_commitment), (att.ct_hash, att.mac));
                }

                for enc in vcs.into_iter().take(MAX_VIEW_CT) {
                    // 1. Check cm is in outputs
                    if !outputs_set.contains(&enc.cm) {
                        return Err(anyhow::anyhow!(
                            "viewer ciphertext cm does not match any transfer outputs"
                        )
                        .into());
                    }

                    // 2. Check (cm, fvk_commitment) exists in attestations
                    let key = (enc.cm, enc.fvk_commitment);
                    let (expected_ct_hash, expected_mac) = att_map.get(&key).ok_or_else(|| {
                        anyhow::anyhow!(
                            "viewer ciphertext (cm={}, fvk_commitment={}) not attested by proof",
                            hex::encode(enc.cm),
                            hex::encode(enc.fvk_commitment)
                        )
                    })?;

                    // 3. Recompute ct_hash from actual ciphertext bytes
                    let ct_h = compute_ct_hash(&enc.ct);

                    // 4. Verify ct_hash matches proof attestation
                    if &ct_h != expected_ct_hash {
                        return Err(anyhow::anyhow!(
                            "ct_hash mismatch: computed {} != proof {}",
                            hex::encode(ct_h),
                            hex::encode(expected_ct_hash)
                        )
                        .into());
                    }

                    // 5. Verify mac matches proof attestation
                    if &enc.mac != expected_mac {
                        return Err(anyhow::anyhow!(
                            "mac mismatch: tx {} != proof {}",
                            hex::encode(enc.mac),
                            hex::encode(expected_mac)
                        )
                        .into());
                    }

                    // All checks passed: emit event
                    self.emit_event(st, Event::NoteEncrypted { enc });
                }
            }

            // Aggregate event (positions are provisional, assigned at flush)
            // Convert view attestations to lightweight viewer bindings for the event
            let viewer_bindings = public.view_attestations.map(|atts| {
                atts.into_iter()
                    .map(|att| crate::event::ViewerBinding {
                        cm: att.cm,
                        fvk_commitment: att.fvk_commitment,
                    })
                    .collect()
            });

            self.emit_event(
                st,
                Event::PoolTransfer {
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                    outputs,
                    viewer_bindings,
                },
            );

            Ok(())
        }
    }

    /// Withdraw: Take money OUT of the privacy pool (shielded → transparent).
    ///
    /// This method atomically:
    /// 1. Verifies a ZK proof
    /// 2. Consumes the input note's nullifier (prevents double-spending)
    /// 3. Creates output note commitments from the proof (for change)
    /// 4. Transfers transparent tokens to the recipient
    ///
    /// SECURITY: anchor_root, nullifier, and withdraw_amount are explicit transaction
    /// fields validated against the proof to prevent tampering.
    pub(crate) fn withdraw(
        &mut self,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))]
        proof: sov_modules_api::SafeVec<u8, 5_000_000>,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] anchor_root: Hash32,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] nullifier: Hash32,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] withdraw_amount: u128,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] to: S::Address,
        #[cfg_attr(not(feature = "native"), allow(unused_variables))] view_ciphertexts: Option<
            Vec<EncryptedNote>,
        >,
        gas: Option<S::Gas>,
        _ctx: &Context<S>,
        st: &mut impl TxState<S>,
    ) -> Result<()> {
        let gas = gas.unwrap_or(<S::Gas as Gas>::zero());
        st.charge_gas(&gas)?;

        #[cfg(not(feature = "native"))]
        {
            anyhow::bail!("Ligero verification requires the \"native\" feature enabled");
        }

        #[cfg(feature = "native")]
        {
            use sov_ligero_adapter::{LigeroCodeCommitment, LigeroVerifier};
            use sov_rollup_interface::zk::{CodeCommitment, ZkVerifier};

            let credential_check_start = std::time::Instant::now();
            let cached_public = crate::get_pre_verified_spend(&nullifier);
            let credential_check_duration = credential_check_start.elapsed();
            debug!(
                credential_check_ms = ?(credential_check_duration.as_secs_f64() * 1000.0),
                has_pre_verified = cached_public.is_some(),
                "Withdraw: checked for pre-verified proof outputs"
            );

            let public = if let Some(public) = cached_public {
                debug!("Using pre-verified path (skipping Ligero proof verification)");
                public
            } else {
                info!("No pre-verified credential, performing full Ligero proof verification");

                let method_id_bytes = self
                    .method_id
                    .get(st)?
                    .ok_or_else(|| anyhow!("method_id not configured in module state"))?;

                let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                    .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;

                LigeroVerifier::verify(&proof, &method_id).map_err(|e| {
                    MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string())
                })?
            };

            // SECURITY: Bind transaction fields to proof-committed values
            if public.anchor_root != anchor_root
                || public.nullifier != nullifier
                || public.withdraw_amount != withdraw_amount
            {
                return Err(MidnightPrivacyError::<S>::PublicOutputMismatch.into());
            }

            // Ensure there's actually a withdrawal
            if withdraw_amount == 0 {
                return Err(anyhow!(
                    "Withdraw must have withdraw_amount > 0. Use Transfer call for pure shielded transactions."
                )
                .into());
            }

            // Limit change outputs to at most 1 (per requirements)
            const MAX_OUTPUTS_WITHDRAW: usize = 1;
            if public.output_commitments.len() > MAX_OUTPUTS_WITHDRAW {
                return Err(MidnightPrivacyError::<S>::TooManyOutputs(
                    public.output_commitments.len(),
                    MAX_OUTPUTS_WITHDRAW,
                )
                .into());
            }

            // 1) Validate anchor root
            if !self.is_valid_anchor(&public.anchor_root, st)? {
                return Err(
                    MidnightPrivacyError::<S>::InvalidAnchorRoot(public.anchor_root).into(),
                );
            }

            // 2) Consume nullifier
            let nk = NullifierKey(public.nullifier);
            if self.nullifier_set.get(&nk, st)?.is_some() {
                return Err(
                    MidnightPrivacyError::<S>::NullifierAlreadySpent(public.nullifier).into(),
                );
            }
            self.nullifier_set.set(&nk, &true, st)?;
            // Stats: bump spent nullifier count
            let n_spent = self.spent_nullifier_count.get(st)?.unwrap_or(0);
            self.spent_nullifier_count.set(&(n_spent + 1), st)?;

            // Record nullifier in the nullifier tree (Aztec-style dual-tree design)
            self.append_nullifier(public.nullifier, st)?;

            // 3) Queue change outputs for end-of-block processing
            let change_outputs: Vec<Hash32> = public.output_commitments.clone();
            for cm in &change_outputs {
                self.add_commitment(*cm, st)?;
            }

            // 4) Transfer transparent tokens
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

            self.bank
                .transfer_from(self.id.to_payable(), &to, coins, st)?;

            // Emit events
            self.emit_event(
                st,
                Event::NoteSpent {
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                },
            );

            // Update withdrawal statistics
            let total_withdrawn = self.total_withdrawn.get(st)?.unwrap_or(0);
            self.total_withdrawn
                .set(&(total_withdrawn + public.withdraw_amount), st)?;

            let withdraw_count = self.withdraw_count.get(st)?.unwrap_or(0);
            self.withdraw_count.set(&(withdraw_count + 1), st)?;

            // Level B: Viewer attestation verification for change outputs
            if let Some(vcs) = view_ciphertexts {
                use crate::viewing::ct_hash as compute_ct_hash;

                const MAX_VIEW_CT: usize = 16; // reasonable upper bound for viewer ciphertexts
                let outputs_set: HashSet<Hash32> = change_outputs.iter().copied().collect();

                // Require Level B attestations when ciphertexts are present
                let attestations = public.view_attestations.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Level B required: proof must include view_attestations when view_ciphertexts are present"
                    )
                })?;

                // Build attestation lookup: (cm, fvk_commitment) -> (ct_hash, mac)
                let mut att_map: std::collections::HashMap<(Hash32, Hash32), (Hash32, Hash32)> =
                    std::collections::HashMap::new();
                for att in attestations {
                    att_map.insert((att.cm, att.fvk_commitment), (att.ct_hash, att.mac));
                }

                for enc in vcs.into_iter().take(MAX_VIEW_CT) {
                    // 1. Check cm is in change outputs
                    if !outputs_set.contains(&enc.cm) {
                        return Err(anyhow::anyhow!(
                            "viewer ciphertext cm does not match any withdraw change outputs"
                        )
                        .into());
                    }

                    // 2. Check (cm, fvk_commitment) exists in attestations
                    let key = (enc.cm, enc.fvk_commitment);
                    let (expected_ct_hash, expected_mac) = att_map.get(&key).ok_or_else(|| {
                        anyhow::anyhow!(
                            "viewer ciphertext (cm={}, fvk_commitment={}) not attested by proof",
                            hex::encode(enc.cm),
                            hex::encode(enc.fvk_commitment)
                        )
                    })?;

                    // 3. Recompute ct_hash from actual ciphertext bytes
                    let ct_h = compute_ct_hash(&enc.ct);

                    // 4. Verify ct_hash matches proof attestation
                    if &ct_h != expected_ct_hash {
                        return Err(anyhow::anyhow!(
                            "ct_hash mismatch: computed {} != proof {}",
                            hex::encode(ct_h),
                            hex::encode(expected_ct_hash)
                        )
                        .into());
                    }

                    // 5. Verify mac matches proof attestation
                    if &enc.mac != expected_mac {
                        return Err(anyhow::anyhow!(
                            "mac mismatch: tx {} != proof {}",
                            hex::encode(enc.mac),
                            hex::encode(expected_mac)
                        )
                        .into());
                    }

                    // All checks passed: emit event
                    self.emit_event(st, Event::NoteEncrypted { enc });
                }
            }

            // Aggregate event (positions are provisional, assigned at flush)
            // Convert view attestations to lightweight viewer bindings for the event
            let viewer_bindings = public.view_attestations.map(|atts| {
                atts.into_iter()
                    .map(|att| crate::event::ViewerBinding {
                        cm: att.cm,
                        fvk_commitment: att.fvk_commitment,
                    })
                    .collect()
            });

            self.emit_event(
                st,
                Event::PoolWithdraw {
                    amount: public.withdraw_amount,
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                    change: change_outputs,
                    viewer_bindings,
                },
            );

            Ok(())
        }
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

    /// End-of-block flush: Replay all pending commitments/nullifiers into their trees.
    /// Called automatically by BlockHooks at the end of each block after all transactions.
    ///
    /// BLOCK-DEFERRED TREE UPDATE: Instead of updating trees per-tx (causing ~4MB writes each),
    /// we queue commitments/nullifiers during txs and replay them here once per block.
    /// This reduces per-tx cache memory from ~4MB to ~100 bytes.
    ///
    /// Process:
    /// 1. Load commitment_tree, replay all pending commitments, record each root
    /// 2. Load nullifier_tree, replay all pending nullifiers
    /// 3. Save both trees once
    ///
    /// SCOPE: Flushes only current block's pending items. Other heights ignored.
    ///
    /// PARALLEL-SAFE: Uses StateMap::iter_prefix to enumerate all commitments/nullifiers
    /// for the current height. Each tx writes to a unique key (height, hash), so there
    /// are no conflicts during parallel execution. Final ordering is deterministic (sorted by hash).
    pub fn end_block_flush(
        &mut self,
        st: &mut sov_modules_api::StateCheckpoint<S>,
    ) -> anyhow::Result<()> {
        let current_height = st.rollup_height_to_access();

        // ═══════════════════════════════════════════════════════════════════════════════
        // PHASE 1: Replay pending commitments into the commitment tree
        // ═══════════════════════════════════════════════════════════════════════════════

        // Collect all commitments using prefix iteration (parallel-safe: no data loss)
        let cm_prefix = crate::hash::PendingCommitmentPrefix {
            height: current_height.get(),
        };
        let cm_entries = self
            .pending_commitments_by_hash
            .iter_prefix(&cm_prefix, st)?;

        // Extract commitments and sort for deterministic ordering
        let mut commitments: Vec<Hash32> = cm_entries
            .into_iter()
            .map(|(key, _)| key.commitment)
            .collect();
        commitments.sort();

        if !commitments.is_empty() {
            let mut tree = self.commitment_tree.get_or_err(st)??;
            let mut pos = self.next_position.get_or_err(st)??;

            for cm in &commitments {
                if pos >= tree.len() as u64 {
                    tree.grow_to_fit((pos + 1) as usize);
                }
                tree.set_leaf(pos as usize, *cm);
                let root = tree.root();
                self.add_recent_root_direct(root, st)?;
                self.record_root_forever_direct(root, st)?;
                pos += 1;
            }

            self.next_position.set(&pos, st)?;
            self.commitment_tree.set(&tree, st)?;

            // Clean up processed entries
            self.pending_commitments_by_hash
                .delete_prefix(&cm_prefix, st)?;

            tracing::debug!(
                height = current_height.get(),
                commitments_flushed = commitments.len(),
                new_next_position = pos,
                "Flushed pending commitments to tree"
            );
        }

        // ═══════════════════════════════════════════════════════════════════════════════
        // PHASE 2: Replay pending nullifiers into the nullifier tree
        // ═══════════════════════════════════════════════════════════════════════════════

        // Collect all nullifiers using prefix iteration
        let nf_prefix = crate::hash::PendingNullifierPrefix {
            height: current_height.get(),
        };
        let nf_entries = self
            .pending_nullifiers_by_hash
            .iter_prefix(&nf_prefix, st)?;

        // Extract nullifiers and sort for deterministic ordering
        let mut nullifiers: Vec<Hash32> = nf_entries
            .into_iter()
            .map(|(key, _)| key.nullifier)
            .collect();
        nullifiers.sort();

        if !nullifiers.is_empty() {
            let mut tree = self.nullifier_tree.get_or_err(st)??;
            let mut pos = self.next_nullifier_position.get_or_err(st)??;

            for nf in &nullifiers {
                if pos >= tree.len() as u64 {
                    tree.grow_to_fit((pos + 1) as usize);
                }
                tree.set_leaf(pos as usize, *nf);
                pos += 1;
            }

            self.next_nullifier_position.set(&pos, st)?;
            self.nullifier_tree.set(&tree, st)?;

            // Clean up processed entries
            self.pending_nullifiers_by_hash
                .delete_prefix(&nf_prefix, st)?;

            tracing::debug!(
                height = current_height.get(),
                nullifiers_flushed = nullifiers.len(),
                new_next_position = pos,
                "Flushed pending nullifiers to tree"
            );
        }

        // ═══════════════════════════════════════════════════════════════════════════════
        // PHASE 3: Legacy - flush any pending roots (for backwards compatibility)
        // This handles roots that were already queued before this change.
        // ═══════════════════════════════════════════════════════════════════════════════
        let root_count = self
            .pending_roots_count
            .get(&current_height, st)?
            .unwrap_or(0);

        for idx in 0..root_count {
            let key = PendingRootKey {
                height: current_height.get(),
                idx,
            };
            let root = self.pending_roots_indexed.get(&key, st)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing indexed root at ({}, {}). State corruption detected.",
                    current_height,
                    idx
                )
            })?;
            self.add_recent_root_direct(root, st)?;
            self.record_root_forever_direct(root, st)?;
        }

        self.pending_roots_count.set(&current_height, &0u32, st)?;

        Ok(())
    }

    /// Helper: Add a root to the recent roots window (for StateCheckpoint).
    fn add_recent_root_direct(
        &mut self,
        root: Hash32,
        state: &mut sov_modules_api::StateCheckpoint<S>,
    ) -> Result<()> {
        let mut recent_roots = self.recent_roots.get_or_err(state)??;
        let root_window_size = self.root_window_size.get_or_err(state)??;

        recent_roots.push_back(root);
        if recent_roots.len() > root_window_size as usize {
            recent_roots.pop_front();
        }

        self.recent_roots.set(&recent_roots, state)?;
        Ok(())
    }

    /// Helper: Record a root in the full-history index (for StateCheckpoint).
    /// Note: This version doesn't emit events since StateCheckpoint doesn't implement EventContainer.
    fn record_root_forever_direct(
        &mut self,
        root: Hash32,
        state: &mut sov_modules_api::StateCheckpoint<S>,
    ) -> Result<()> {
        // Fast path: already recorded?
        if self.all_roots.get(&RootKey(root), state)?.is_some() {
            return Ok(());
        }

        // Assign a monotonic sequence number and commit to permanent storage
        let seq = self.root_seq.get_or_err(state)??;
        self.all_roots.set(&RootKey(root), &seq, state)?;

        // Note: We don't emit root events here because StateCheckpoint doesn't implement EventContainer.
        // Root publication happens silently during flush; clients query state for current roots.

        // Bump sequence (checked add to be safe against overflow)
        let next_seq = seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("root_seq overflow: too many unique roots"))?;
        self.root_seq.set(&next_seq, state)?;

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
            return Err(MidnightPrivacyError::WrongSender::<S> {
                admin,
                sender: context.sender().clone(),
            }
            .into());
        }

        // Update the method ID
        self.method_id.set(&new_method_id, state)?;

        // Emit event
        self.emit_event(state, Event::MethodIdUpdated { new_method_id });

        Ok(())
    }
}
