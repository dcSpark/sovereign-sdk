use std::fmt::Debug;

use anyhow::{Result, anyhow};
use schemars::JsonSchema;
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::{Context, EventEmitter, Gas, Spec, TxState, StateReaderAndWriter};
// no additional imports
use thiserror::Error;
use tracing::{debug, info};
use std::collections::HashSet;

use super::{PreVerifiedWithdrawCredential, ValueMidnightPrivacy};
use crate::event::Event;
use crate::hash::{note_commitment, Hash32, RootKey};
use crate::types::{EncryptedNote, FullViewingKey};
// NEW
use crate::pending::PendingOutput;
use sov_rollup_interface::common::HexHash;

// anyhow is used unconditionally for error construction

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

    /// Number of output commitments in proof exceeds maximum allowed
    #[error("Too many output commitments: {0} (max: {1})")]
    TooManyOutputs(usize, usize),
}

impl<S: Spec> ValueMidnightPrivacy<S> {
    // --- NEW: append helper ---
    // Note: legacy queue_output using shared `pending_log` removed in favor of per-tx outbox.

    /// Queue an output commitment keyed by a per-transaction identifier to avoid shared writes.
    fn queue_output_by_txid(
        &mut self,
        txid: HexHash,
        cm: Hash32,
        st: &mut impl TxState<S>,
    ) -> Result<()> {
        let mut outbox = self
            .pending_by_tx
            .get(&txid, st)?
            .unwrap_or_else(|| Vec::new());
        outbox.push(PendingOutput { cm });
        self.pending_by_tx.set(&txid, &outbox, st)?;

        self.emit_event(st, Event::NoteQueued { commitment: cm });
        Ok(())
    }

    // --- NEW: epilogue ---
    /// Implementation of apply_pending without requiring TxState-specific capabilities.
    pub(crate) fn apply_pending_inner<WS>(&mut self, st: &mut WS) -> Result<()>
    where
        WS: StateReaderAndWriter<sov_state::namespaces::User>,
    {
        // Apply per-tx outboxes strictly according to block order; if no order, do nothing.
        let tx_count = self.block_tx_order.len(st)?;
        if tx_count == 0 {
            return Ok(());
        }

        let mut tree = self.commitment_tree.get_or_err(st)??;
        let mut pos = self.next_position.get_or_err(st)??;
        let mut agg_stats = crate::PendingStats::default();

        for i in 0..tx_count {
            let Some(txid) = self.block_tx_order.get(i, st)? else { continue };
            if let Some(outbox) = self.pending_by_tx.get(&txid, st)? {
                for PendingOutput { cm } in &outbox {
                    if (pos as usize) >= tree.len() {
                        return Err(MidnightPrivacyError::<S>::TreeFull(tree.len()).into());
                    }
                    tree.set_leaf(pos as usize, *cm);
                    pos += 1;
                }

                // cleanup
                self.pending_by_tx.delete(&txid, st)?;
            }

            // fold stats for this tx, if present
            if let Some(ps) = self.pending_stats_by_tx.get(&txid, st)? {
                agg_stats.deposits += ps.deposits;
                agg_stats.deposited_amt = agg_stats.deposited_amt.saturating_add(ps.deposited_amt);
                agg_stats.withdraws += ps.withdraws;
                agg_stats.withdrawn_amt =
                    agg_stats.withdrawn_amt.saturating_add(ps.withdrawn_amt);
                agg_stats.spent_nullifiers += ps.spent_nullifiers;
                self.pending_stats_by_tx.delete(&txid, st)?;
            }
        }

        self.next_position.set(&pos, st)?;
        let new_root = tree.root();
        self.commitment_tree.set(&tree, st)?;
        self.add_recent_root(new_root, st)?;
        self.record_root_forever(new_root, st)?;

        // Note: no events here; hook path cannot emit. Call path emits after invoking inner.

        // Update global counters once
        if agg_stats.deposits != 0 {
            let c = self.deposit_count.get(st)?.unwrap_or(0);
            self.deposit_count.set(&(c + agg_stats.deposits), st)?;
        }
        if agg_stats.deposited_amt != 0 {
            let t = self.total_deposited.get(st)?.unwrap_or(0);
            self.total_deposited.set(&(t + agg_stats.deposited_amt), st)?;
        }
        if agg_stats.withdraws != 0 {
            let c = self.withdraw_count.get(st)?.unwrap_or(0);
            self.withdraw_count.set(&(c + agg_stats.withdraws), st)?;
        }
        if agg_stats.withdrawn_amt != 0 {
            let t = self.total_withdrawn.get(st)?.unwrap_or(0);
            self.total_withdrawn.set(&(t + agg_stats.withdrawn_amt), st)?;
        }
        if agg_stats.spent_nullifiers != 0 {
            let c = self.spent_nullifier_count.get(st)?.unwrap_or(0);
            self.spent_nullifier_count.set(&(c + agg_stats.spent_nullifiers), st)?;
        }

        // Clear block order vector robustly
        self.block_tx_order.clear(st)?;

        Ok(())
    }

    /// Legacy helper: direct per-tx tree append.
    /// Not used in deferred mode. Keep for migration tests or delete.
    #[allow(dead_code)]
    fn add_commitment(
        &mut self,
        commitment: Hash32,
        state: &mut impl TxState<S>,
    ) -> Result<(u64, Hash32)> {
        // Get current tree and position
        let mut tree = self.commitment_tree.get_or_err(state)??;
        let position = self.next_position.get_or_err(state)??;

        // Check if tree is full
        if position >= tree.len() as u64 {
            return Err(MidnightPrivacyError::<S>::TreeFull(tree.len()).into());
        }

        // Add to tree
        tree.set_leaf(position as usize, commitment);
        let new_root = tree.root();

        // Update state
        self.commitment_tree.set(&tree, state)?;
        self.next_position.set(&(position + 1), state)?;

        // 1) Add root to recent roots window (fast mempool checks)
        self.add_recent_root(new_root, state)?;
        // 2) Permanently record in NOMT-backed index (full history for long-range anchors)
        self.record_root_forever(new_root, state)?;

        // Emit event
        self.emit_event(
            state,
            Event::NoteCreated {
                commitment,
                position,
                new_root,
            },
        );

        Ok((position, new_root))
    }

    /// Deposit: transfer tokens into the pool, append commitment, update root window.
    /// Deposit: now queues the commitment; Merkle updates happen in epilogue.
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

        // Bank transfer unchanged
        use sov_bank::IntoPayable;
        let amount_u64: u64 = amount
            .try_into()
            .map_err(|_| MidnightPrivacyError::<S>::AmountOverflow(amount))?;
        let token_id = self.token_id.get_or_err(st)??;
        self.bank.transfer_from(
            ctx.sender(),
            self.id.to_payable(),
            sov_bank::Coins { amount: sov_bank::Amount::from(amount_u64), token_id },
            st,
        )?;

        // Compute commitment and queue
        let domain = self.domain.get_or_err(st)??;
        let cm = note_commitment(&domain, amount, &rho, &recipient);
        let txid = ctx.tx_hash();
        self.queue_output_by_txid(txid, cm, st)?;

        // Optional viewer ciphertexts unchanged
        if let Some(fvks) = view_fvks {
            const MAX_VIEW_CT: usize = 8;
            let note = crate::types::Note { domain, value: amount, rho, recipient };
            for fvk in fvks.into_iter().take(MAX_VIEW_CT) {
                let enc = crate::viewing::encrypt_note_for_fvk(&fvk, &note, &cm)
                    .map_err(|e| anyhow::anyhow!("viewer encrypt (deposit): {e}"))?;
                self.emit_event(st, Event::NoteEncrypted { enc });
            }
        }

        // Per-tx stats
        let mut ps = self
            .pending_stats_by_tx
            .get(&txid, st)?
            .unwrap_or_default();
        ps.deposits += 1;
        ps.deposited_amt = ps.deposited_amt.saturating_add(amount);
        self.pending_stats_by_tx.set(&txid, &ps, st)?;

        // Non-positional deposit event
        self.emit_event(st, Event::PoolDeposit { amount, commitment: cm });

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
        #[cfg_attr(not(feature = "native"), allow(unused_variables))]
        view_ciphertexts: Option<Vec<EncryptedNote>>,
        gas: Option<S::Gas>,
        ctx: &Context<S>,
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

            let method_id_bytes = self
                .method_id
                .get(st)?
                .ok_or_else(|| anyhow!("method_id not configured in module state"))?;

            let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;

            // Try to use pre-verified credential (preferred fast path)
            let credential_check_start = std::time::Instant::now();
            let ctx_public = ctx
                .get_sender_credential::<PreVerifiedWithdrawCredential>()
                .map(|cred| cred.0.clone());
            let cached_public = crate::get_pre_verified_spend(&nullifier);
            let has_credential = ctx_public.is_some() || cached_public.is_some();
            let credential_check_duration = credential_check_start.elapsed();
            debug!(
                credential_check_ms = ?(credential_check_duration.as_secs_f64() * 1000.0),
                has_credential = ?has_credential,
                "Transfer: checked for pre-verified credential"
            );

            let public = if let Some(public) = ctx_public.or(cached_public) {
                info!("Using pre-verified credential path (skipping Ligero proof verification)");
                public
            } else {
                info!("No pre-verified credential, performing full Ligero proof verification");
                // Verify the proof and extract public output
                LigeroVerifier::verify(&proof, &method_id)
                    .map_err(|e| MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string()))?
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
            // Per-tx stats: bump spent nullifier count
            let txid = ctx.tx_hash();
            let mut ps = self
                .pending_stats_by_tx
                .get(&txid, st)?
                .unwrap_or_default();
            ps.spent_nullifiers += 1;
            self.pending_stats_by_tx.set(&txid, &ps, st)?;
            // Cleanup any cached pre-verified entry
            crate::clear_pre_verified_spend(&public.nullifier);

            // 3) Queue outputs; positions are assigned in the epilogue
            for cm in &public.output_commitments {
                self.queue_output_by_txid(txid, *cm, st)?;
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
                
                const MAX_VIEW_CT: usize = 16;
                let outputs_set: HashSet<Hash32> =
                    public.output_commitments.iter().copied().collect();

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

            // Aggregate, non-positional
            self.emit_event(
                st,
                Event::PoolTransfer {
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                    outputs: public.output_commitments.clone(),
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
        #[cfg_attr(not(feature = "native"), allow(unused_variables))]
        view_ciphertexts: Option<Vec<EncryptedNote>>,
        gas: Option<S::Gas>,
        ctx: &Context<S>,
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
            let ctx_public = ctx
                .get_sender_credential::<PreVerifiedWithdrawCredential>()
                .map(|cred| cred.0.clone());
            let cached_public = crate::get_pre_verified_spend(&nullifier);
            let has_credential = ctx_public.is_some() || cached_public.is_some();
            let credential_check_duration = credential_check_start.elapsed();
            debug!(
                credential_check_ms = ?(credential_check_duration.as_secs_f64() * 1000.0),
                has_credential = ?has_credential,
                "Withdraw: checked for pre-verified credential"
            );

            let public = if let Some(public) = ctx_public.or(cached_public) {
                info!("Using pre-verified credential path (skipping Ligero proof verification)");
                public
            } else {
                info!("No pre-verified credential, performing full Ligero proof verification");

                let method_id_bytes = self
                    .method_id
                    .get(st)?
                    .ok_or_else(|| anyhow!("method_id not configured in module state"))?;

                let method_id = LigeroCodeCommitment::decode(&method_id_bytes)
                    .map_err(|e| anyhow!("Invalid method_id bytes in state: {}", e))?;

                LigeroVerifier::verify(&proof, &method_id)
                    .map_err(|e| MidnightPrivacyError::<S>::ProofVerificationFailed(e.to_string()))?
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
            // Per-tx stats: bump spent nullifier count
            let txid = ctx.tx_hash();
            let mut ps = self
                .pending_stats_by_tx
                .get(&txid, st)?
                .unwrap_or_default();
            ps.spent_nullifiers += 1;
            self.pending_stats_by_tx.set(&txid, &ps, st)?;
            // Cleanup cached pre-verified entry
            crate::clear_pre_verified_spend(&public.nullifier);

            // 3) Queue change outputs; positions assigned in epilogue
            for cm in &public.output_commitments {
                self.queue_output_by_txid(txid, *cm, st)?;
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

            // Per-tx withdrawal statistics
            let mut psw = self
                .pending_stats_by_tx
                .get(&txid, st)?
                .unwrap_or_default();
            psw.withdraws += 1;
            psw.withdrawn_amt =
                psw.withdrawn_amt.saturating_add(public.withdraw_amount);
            self.pending_stats_by_tx.set(&txid, &psw, st)?;

            // Level B: Viewer attestation verification for change outputs
            if let Some(vcs) = view_ciphertexts {
                use crate::viewing::ct_hash as compute_ct_hash;
                
                const MAX_VIEW_CT: usize = 16;
                let outputs_set: HashSet<Hash32> =
                    public.output_commitments.iter().copied().collect();

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

            // Aggregate, non-positional
            self.emit_event(
                st,
                Event::PoolWithdraw {
                    amount: public.withdraw_amount,
                    nullifier: public.nullifier,
                    anchor_root: public.anchor_root,
                    change: public.output_commitments.clone(),
                },
            );

            Ok(())
        }
    }

    /// Add a root to the recent roots window (circular buffer).
    /// Uses VecDeque for O(1) operations at both ends.
    /// This provides fast mempool checks for recent transactions.
    pub(crate) fn add_recent_root<RW>(&mut self, root: Hash32, state: &mut RW) -> Result<()>
    where
        RW: StateReaderAndWriter<sov_state::namespaces::User>,
    {
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
    pub(crate) fn record_root_forever<RW>(&mut self, root: Hash32, state: &mut RW) -> Result<()>
    where
        RW: StateReaderAndWriter<sov_state::namespaces::User>,
    {
        // Fast path: already recorded?
        if self.all_roots.get(&RootKey(root), state)?.is_some() {
            return Ok(());
        }

        // Assign a monotonic sequence number and commit to permanent storage
        let seq = self.root_seq.get_or_err(state)??;
        self.all_roots.set(&RootKey(root), &seq, state)?;

        // Bump sequence (checked add to be safe against overflow)
        let next_seq = seq
            .checked_add(1)
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
        // Test helper: allow bypassing anchor validation when mocking verification in e2e tests.
        // Either explicit module flag or the Ligero mock flag will short-circuit to true.
        let mock_anchor_ok = std::env::var("MIDNIGHT_PRIVACY_MOCK_ANCHOR")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false)
            || std::env::var("LIGERO_MOCK_VERIFY")
                .ok()
                .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
                .unwrap_or(false);
        if mock_anchor_ok {
            return Ok(true);
        }

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
