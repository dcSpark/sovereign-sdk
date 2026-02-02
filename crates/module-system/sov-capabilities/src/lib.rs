use std::convert::Infallible;

#[cfg(feature = "native")]
use sov_attester_incentives::BondingProofServiceImpl;
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use sov_bank::utils::TokenHolder;
use sov_bank::{config_gas_token_id, Coins, IntoPayable, Payable};
#[cfg(feature = "native")]
use sov_modules_api::capabilities::HasKernel;
use sov_modules_api::capabilities::{
    AuthorizationData, GasEnforcer, ProofProcessor, SequencerAuthorization, SequencerRemuneration,
    TransactionAuthorizer,
};
use sov_modules_api::transaction::{
    AuthenticatedTransactionData, ProverReward, RemainingFunds, SequencerReward,
};
use sov_modules_api::ExecutionContext;
use sov_modules_api::{
    AggregatedProofPublicData, Amount, Context, DaSpec, Gas, GetGasPrice, InfallibleStateAccessor,
    InvalidProofError, ModuleInfo, OperatingMode, Rewards, SovAttestation,
    SovStateTransitionPublicData, Spec, StateAccessor, StateReader, StateWriter, Storage, TxState,
};
use sov_rollup_interface::common::SlotNumber;
use sov_rollup_interface::tee::{
    SerializedTEEAttestation, TeeOracleSignedMAAAttestationV1, TEEAttestation,
    TEE_ORACLE_STATEMENT_DOMAIN_V1,
};
use sov_rollup_interface::zk::aggregated_proof::SerializedAggregatedProof;
#[cfg(feature = "native")]
use sov_rollup_interface::StateUpdateInfo;
use sov_sequencer_registry::SequencerRegistry;
use sov_state::{Kernel, User};

/// Implements the basic capabilities required for a zk-rollup runtime.
pub struct StandardProvenRollupCapabilities<'a, S: Spec, GasPayer = ()> {
    pub bank: &'a mut sov_bank::Bank<S>,
    pub gas_payer: GasPayer,
    pub sequencer_registry: &'a mut SequencerRegistry<S>,
    pub accounts: &'a mut sov_accounts::Accounts<S>,
    pub uniqueness: &'a mut sov_uniqueness::Uniqueness<S>,
    pub operator_incentives: &'a mut sov_operator_incentives::OperatorIncentives<S>,
    pub prover_incentives: &'a mut sov_prover_incentives::ProverIncentives<S>,
    pub attester_incentives: &'a mut sov_attester_incentives::AttesterIncentives<S>,
}

impl<'a, S: Spec, T> StandardProvenRollupCapabilities<'a, S, T> {
    fn get_prover_token_holder(
        &'a self,
        oprating_mode: OperatingMode,
        state: &mut impl InfallibleStateAccessor,
    ) -> TokenHolder<S> {
        let rewarded_token_holder = match oprating_mode {
            OperatingMode::TEE => self.prover_incentives.id().to_payable().into(),
            OperatingMode::Zk => self.prover_incentives.id().to_payable().into(),
            OperatingMode::Optimistic => self.attester_incentives.id().to_payable().into(),
            OperatingMode::Operator => {
                let addr = self.operator_incentives.reward_address(state);
                TokenHolder::User(addr)
            }
        };

        rewarded_token_holder
    }
}

trait HasGasPayer<S: Spec> {
    fn try_reserve_gas_from_payer(
        &mut self,
        tx: &AuthenticatedTransactionData<S>,
        gas_price: &<S::Gas as Gas>::Price,
        context: &mut Context<S>,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<()>;
}

impl<S: Spec> HasGasPayer<S> for StandardProvenRollupCapabilities<'_, S> {
    /// Reserves enough gas for the transaction to be processed, if possible.
    fn try_reserve_gas_from_payer(
        &mut self,
        tx: &AuthenticatedTransactionData<S>,
        gas_price: &<S::Gas as Gas>::Price,
        context: &mut Context<S>,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<()> {
        self.bank
            .reserve_gas(tx, gas_price, context.sender(), state)
            .map_err(Into::into)
    }
}

impl<'a, S: Spec> HasGasPayer<S>
    for StandardProvenRollupCapabilities<'a, S, &'a mut sov_paymaster::Paymaster<S>>
{
    /// Reserves enough gas for the transaction to be processed, if possible.
    fn try_reserve_gas_from_payer(
        &mut self,
        tx: &AuthenticatedTransactionData<S>,
        gas_price: &<S::Gas as Gas>::Price,
        context: &mut Context<S>,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<()> {
        self.gas_payer
            .try_reserve_gas(tx, gas_price, context, state)
            .map_err(Into::into)
    }
}

fn gas_coins(amount: Amount) -> Coins {
    Coins {
        amount,
        token_id: config_gas_token_id(),
    }
}

impl<S: Spec, T> GasEnforcer<S> for StandardProvenRollupCapabilities<'_, S, T>
where
    Self: HasGasPayer<S>,
{
    /// Reserves enough gas for the transaction to be processed, if possible.
    fn try_reserve_gas(
        &mut self,
        tx: &AuthenticatedTransactionData<S>,
        gas_price: &<S::Gas as Gas>::Price,
        context: &mut Context<S>,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<()> {
        self.try_reserve_gas_from_payer(tx, gas_price, context, state)
    }

    fn try_reserve_gas_for_proof(
        &mut self,
        tx: &AuthenticatedTransactionData<S>,
        gas_price: &<S::Gas as Gas>::Price,
        sender: &S::Address,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<()> {
        self.bank
            .reserve_gas(tx, gas_price, sender, state)
            .map_err(Into::into)
    }

    fn reward_prover(
        &mut self,
        prover_rewards: &ProverReward,
        oprating_mode: OperatingMode,
        state: &mut impl InfallibleStateAccessor,
    ) {
        let rewarded_module = self.get_prover_token_holder(oprating_mode, state);

        self.bank
            .transfer_from(
                self.bank.id.clone().to_payable(),
                rewarded_module.to_owned().as_token_holder(),
                Coins {
                    amount: prover_rewards.0,
                    token_id: config_gas_token_id(),
                },
                state,
            )
            // SAFETY: It is safe to unwrap here because the caller must ensure that sufficient funds are reserved.
            .expect("Caller failed to ensure sufficient funds are reserved, but this is required for reward_prover to remain infallible");
    }

    fn refund_remaining_gas(
        &mut self,
        recipient: &S::Address,
        remaining_funds: &RemainingFunds,
        state: &mut impl InfallibleStateAccessor,
    ) {
        // We refund the payer. We need to give back the remaining funds on the gas meter, plus the unspent tip.
        // This is also the maximum fee minus everything that was spent for the tip and base fee (ie the total reward).
        self.bank
            .transfer_from(
                self.bank.id.clone().to_payable(),
                recipient,
                gas_coins(remaining_funds.0),
                state,
            )
            // SAFETY: It is safe to unwrap here because the caller must ensure that sufficient funds are reserved.
            .expect("Caller failed to ensure sufficient funds are reserved, but this is required for refund_remaining_gas to remain infallible");
    }

    fn reward_prover_from_sequencer_balance(
        &mut self,
        amount: Amount,
        _sequencer: &S::Address,
        oprating_mode: OperatingMode,
        state: &mut impl InfallibleStateAccessor,
    ) -> anyhow::Result<()> {
        let rewarded_prover_module = self.get_prover_token_holder(oprating_mode, state);
        // Transfer the penalty from the sequencer bank to the sequencer
        self.bank.transfer_from(
            self.bank.id.clone().to_payable(),
            rewarded_prover_module.to_owned(),
            gas_coins(amount),
            state,
        )
    }

    fn return_escrowed_funds_to_sequencer<
        Accessor: StateReader<Kernel, Error = Infallible>
            + StateWriter<Kernel, Error = Infallible>
            + StateWriter<User, Error = Infallible>
            + StateReader<User, Error = Infallible>,
    >(
        &mut self,
        bond_amount: Amount,
        reward: Rewards,
        sequencer: &<S::Da as DaSpec>::Address,
        state: &mut Accessor,
    ) {
        let mut net_amount = bond_amount.checked_sub(reward.accumulated_penalty).expect("A sequencer can never be penalized more than the amount they have escrowed, regardless of reward accumulation!");
        net_amount = net_amount.checked_add(reward.accumulated_reward).expect("Total sequencer reward + escrow amount is greater than the max possible token supply. This is a bug in gas accounting.");

        self.sequencer_registry.add_to_stake(
            self.bank.id().to_payable(),
            sequencer,
            net_amount,
            state,
        ).expect("Attempted to send more funds to the sequencer than they have escrowed. This is a bug in gas accounting.");
    }
}

impl<S: Spec, T> SequencerAuthorization<S> for StandardProvenRollupCapabilities<'_, S, T> {
    fn is_preferred_sequencer(
        &self,
        sequencer: &<S::Da as DaSpec>::Address,
        state: &mut impl InfallibleStateAccessor,
    ) -> bool {
        self.sequencer_registry.preferred_sequencer(state).as_ref() == Some(sequencer)
    }
}

impl<S: Spec, T> TransactionAuthorizer<S> for StandardProvenRollupCapabilities<'_, S, T> {
    /// Prevents duplicate transactions from running.
    fn check_uniqueness(
        &self,
        auth_data: &AuthorizationData<S>,
        _context: &Context<S>,
        execution_context: &ExecutionContext,
        state: &mut impl StateReader<User>,
    ) -> anyhow::Result<()> {
        self.uniqueness.check_uniqueness(
            &auth_data.credential_id,
            auth_data.uniqueness,
            auth_data.tx_hash,
            execution_context,
            state,
        )
    }

    /// Marks a transaction as having been executed, preventing it from executing again.
    fn mark_tx_attempted(
        &mut self,
        auth_data: &AuthorizationData<S>,
        _sequencer: &<S::Da as DaSpec>::Address,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<()> {
        self.uniqueness.mark_tx_attempted(
            &auth_data.credential_id,
            auth_data.uniqueness,
            auth_data.tx_hash,
            state,
        )
    }

    /// Resolves the context for a transaction.
    fn resolve_context(
        &mut self,
        auth_data: &AuthorizationData<S>,
        sequencer: &<S::Da as DaSpec>::Address,
        sequencer_rollup_address: S::Address,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<Context<S>> {
        // This should be resolved by the sequencer registry during blob selection
        let sender = self.accounts.resolve_sender_address(
            &auth_data.default_address,
            &auth_data.credential_id,
            state,
        )?;
        Ok(Context::new(
            sender,
            auth_data.credentials.clone(),
            sequencer_rollup_address,
            sequencer.clone(),
        ))
    }

    fn resolve_unregistered_context(
        &mut self,
        auth_data: &AuthorizationData<S>,
        sequencer: &<<S as Spec>::Da as DaSpec>::Address,
        state: &mut impl StateAccessor,
    ) -> anyhow::Result<Context<S>> {
        let sender = self.accounts.resolve_sender_address(
            &auth_data.default_address,
            &auth_data.credential_id,
            state,
        )?;
        // The tx sender & sequencer are the same entity
        Ok(Context::new(
            sender.clone(),
            auth_data.credentials.clone(),
            sender,
            sequencer.clone(),
        ))
    }
}

impl<S: Spec, T> ProofProcessor<S> for StandardProvenRollupCapabilities<'_, S, T> {
    #[cfg(feature = "native")]
    type BondingProofService<K: HasKernel<S>> = BondingProofServiceImpl<S, K>;

    #[cfg(feature = "native")]
    fn create_bonding_proof_service<K: HasKernel<S>>(
        &self,
        attester_address: <S as Spec>::Address,
        state_update_info: sov_modules_api::prelude::tokio::sync::watch::Receiver<
            StateUpdateInfo<<S as Spec>::Storage>,
        >,
    ) -> Self::BondingProofService<K> {
        use sov_attester_incentives::BondingProofServiceImpl;

        BondingProofServiceImpl::new(
            attester_address,
            self.attester_incentives.clone(),
            state_update_info,
        )
    }

    #[allow(clippy::type_complexity)]
    fn process_aggregated_proof<ST: TxState<S> + GetGasPrice<Spec = S>>(
        &mut self,
        proof: SerializedAggregatedProof,
        prover_address: &S::Address,
        state: &mut ST,
    ) -> Result<
        (
            AggregatedProofPublicData<S::Address, S::Da, <S::Storage as Storage>::Root>,
            SerializedAggregatedProof,
        ),
        InvalidProofError,
    > {
        let result = self
            .prover_incentives
            .process_proof(&proof, prover_address, state)?;

        Ok((result, proof))
    }

    fn process_tee_attestation<ST: TxState<S> + GetGasPrice<Spec = S>>(
        &mut self,
        proof: SerializedTEEAttestation,
        prover_address: &S::Address,
        state: &mut ST,
    ) -> Result<
        (
            AggregatedProofPublicData<S::Address, S::Da, <S::Storage as Storage>::Root>,
            TEEAttestation,
        ),
        InvalidProofError,
    > {
        let att: TEEAttestation = borsh::from_slice(&proof.tee_raw_attestation).map_err(|e| {
            InvalidProofError::PreconditionNotMet(format!(
                "Invalid TEE attestation payload: {e}"
            ))
        })?;

        let allowed_oracle_pubkeys = self
            .prover_incentives
            .tee_oracle_pubkeys
            .get(state)
            .map_err(|e| InvalidProofError::StateAccess(format!("{e:?}")))?
            .unwrap_or_default();

        verify_oracle_signed_maa_attestation_v1(&att, &allowed_oracle_pubkeys)?;

        // Reuse the existing aggregated-proof public data verification logic (range checks, state root checks, etc.)
        // but return a TEE receipt so the attestation becomes the first-class proof artifact in the STF.
        let agg_proof = SerializedAggregatedProof {
            raw_aggregated_proof: att.raw_aggregated_proof.clone(),
        };

        let pub_data = self
            .prover_incentives
            .process_proof(&agg_proof, prover_address, state)?;

        // Ensure attested batch public data matches the public data extracted from the aggregated-proof wrapper.
        if pub_data.initial_state_root.as_ref() != att.batch_data.prev_state_root.as_ref() {
            return Err(InvalidProofError::PreconditionNotMet(
                "TEE batch data prev_state_root does not match aggregated public data initial_state_root"
                    .to_owned(),
            ));
        }
        if pub_data.final_state_root.as_ref() != att.batch_data.post_state_root.as_ref() {
            return Err(InvalidProofError::PreconditionNotMet(
                "TEE batch data post_state_root does not match aggregated public data final_state_root"
                    .to_owned(),
            ));
        }
        if pub_data.withdraw_root != att.batch_data.withdraw_root {
            return Err(InvalidProofError::PreconditionNotMet(
                "TEE batch data withdraw_root does not match aggregated public data withdraw_root"
                    .to_owned(),
            ));
        }
        if pub_data.message_queue_hash != att.batch_data.message_queue_hash {
            return Err(InvalidProofError::PreconditionNotMet(
                "TEE batch data message_queue_hash does not match aggregated public data message_queue_hash"
                    .to_owned(),
            ));
        }

        Ok((pub_data, att))
    }

    fn process_attestation<ST: TxState<S> + GetGasPrice<Spec = S>>(
        &mut self,
        proof: sov_rollup_interface::optimistic::SerializedAttestation,
        prover_address: &<S as Spec>::Address,
        state: &mut ST,
    ) -> Result<SovAttestation<S>, InvalidProofError> {
        let result = self
            .attester_incentives
            .process_attestation(prover_address, proof, state)?;

        Ok(result)
    }

    fn process_challenge<ST: TxState<S> + GetGasPrice<Spec = S>>(
        &mut self,
        proof: sov_rollup_interface::optimistic::SerializedChallenge,
        rollup_height: SlotNumber,
        prover_address: &<S as Spec>::Address,
        state: &mut ST,
    ) -> Result<SovStateTransitionPublicData<S>, InvalidProofError> {
        let result = self.attester_incentives.process_challenge(
            prover_address,
            &proof,
            rollup_height,
            state,
        )?;

        Ok(result)
    }
}

fn verify_oracle_signed_maa_attestation_v1(
    att: &TEEAttestation,
    allowed_oracle_pubkeys: &[[u8; 32]],
) -> Result<(), InvalidProofError> {
    if allowed_oracle_pubkeys.is_empty() {
        return Err(InvalidProofError::PreconditionNotMet(
            "No TEE oracle public keys configured in prover incentives genesis".to_owned(),
        ));
    }

    if att.attestation_type != sov_modules_api::TEEAttestationType::MAA {
        return Err(InvalidProofError::PreconditionNotMet(
            "Unsupported TEE attestation type (expected MAA)".to_owned(),
        ));
    }

    let signed: TeeOracleSignedMAAAttestationV1 = borsh::from_slice(&att.attestation).map_err(|e| {
        InvalidProofError::PreconditionNotMet(format!(
            "Invalid oracle-signed MAA attestation payload: {e}"
        ))
    })?;

    if signed.statement.domain != TEE_ORACLE_STATEMENT_DOMAIN_V1 {
        return Err(InvalidProofError::PreconditionNotMet(
            "Invalid oracle statement domain".to_owned(),
        ));
    }

    if signed.statement.attestation_type != att.attestation_type {
        return Err(InvalidProofError::PreconditionNotMet(
            "Oracle statement attestation_type does not match outer attestation_type".to_owned(),
        ));
    }

    if signed.statement.batch_data != att.batch_data {
        return Err(InvalidProofError::PreconditionNotMet(
            "Oracle statement batch_data does not match outer batch_data".to_owned(),
        ));
    }

    let expected_proof_hash: [u8; 32] = Sha256::digest(&att.raw_aggregated_proof).into();
    if signed.statement.raw_aggregated_proof_sha256 != expected_proof_hash {
        return Err(InvalidProofError::PreconditionNotMet(
            "Oracle statement raw_aggregated_proof_sha256 mismatch".to_owned(),
        ));
    }

    let expected_jwt_hash: [u8; 32] = Sha256::digest(signed.attestation_jwt.as_bytes()).into();
    if signed.statement.attestation_jwt_sha256 != expected_jwt_hash {
        return Err(InvalidProofError::PreconditionNotMet(
            "Oracle statement attestation_jwt_sha256 mismatch".to_owned(),
        ));
    }

    if !allowed_oracle_pubkeys.contains(&signed.oracle_pubkey) {
        return Err(InvalidProofError::PreconditionNotMet(
            "Oracle pubkey is not authorized by genesis".to_owned(),
        ));
    }

    let message = borsh::to_vec(&signed.statement).map_err(|e| {
        InvalidProofError::PreconditionNotMet(format!(
            "Failed to serialize oracle statement for signature verification: {e}"
        ))
    })?;

    let verifying_key = VerifyingKey::from_bytes(&signed.oracle_pubkey).map_err(|e| {
        InvalidProofError::PreconditionNotMet(format!("Invalid oracle pubkey bytes: {e}"))
    })?;

    verifying_key
        .verify_strict(&message, &Ed25519Signature::from_bytes(&signed.oracle_signature))
        .map_err(|e| {
            InvalidProofError::PreconditionNotMet(format!(
                "Invalid oracle signature over TEE statement: {e}"
            ))
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::Signer;

    fn make_attestation(
        signing_key: &SigningKey,
        raw_aggregated_proof: Vec<u8>,
    ) -> (TEEAttestation, Vec<[u8; 32]>) {
        let mut batch_data = sov_rollup_interface::tee::TEEAttestation::default().batch_data;
        batch_data.layer2_chain_id = 1337;
        batch_data.batch_index = 7;

        let attestation_jwt = "mock-jwt".to_owned();
        let statement = sov_rollup_interface::tee::TeeOracleStatementV1 {
            domain: TEE_ORACLE_STATEMENT_DOMAIN_V1,
            attestation_type: sov_modules_api::TEEAttestationType::MAA,
            batch_data: batch_data.clone(),
            raw_aggregated_proof_sha256: Sha256::digest(&raw_aggregated_proof).into(),
            attestation_jwt_sha256: Sha256::digest(attestation_jwt.as_bytes()).into(),
        };

        let message = borsh::to_vec(&statement).unwrap();
        let sig = signing_key.sign(&message).to_bytes();

        let signed = TeeOracleSignedMAAAttestationV1 {
            attestation_jwt,
            statement,
            oracle_pubkey: *signing_key.verifying_key().as_bytes(),
            oracle_signature: sig,
        };

        let att = TEEAttestation {
            attestation: borsh::to_vec(&signed).unwrap(),
            raw_aggregated_proof,
            batch_data,
            attestation_type: sov_modules_api::TEEAttestationType::MAA,
        };

        (att, vec![*signing_key.verifying_key().as_bytes()])
    }

    #[test]
    fn oracle_signed_maa_attestation_verifies() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let (att, allowed) = make_attestation(&signing_key, vec![1, 2, 3, 4]);
        verify_oracle_signed_maa_attestation_v1(&att, &allowed).unwrap();
    }

    #[test]
    fn oracle_signed_maa_attestation_rejects_bad_signature() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let (mut att, allowed) = make_attestation(&signing_key, vec![1, 2, 3, 4]);

        let mut signed: TeeOracleSignedMAAAttestationV1 = borsh::from_slice(&att.attestation).unwrap();
        signed.oracle_signature[0] ^= 0x01;
        att.attestation = borsh::to_vec(&signed).unwrap();

        assert!(verify_oracle_signed_maa_attestation_v1(&att, &allowed).is_err());
    }

    #[test]
    fn oracle_signed_maa_attestation_rejects_unauthorized_pubkey() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let (att, _) = make_attestation(&signing_key, vec![1, 2, 3, 4]);

        let other_key = SigningKey::from_bytes(&[9u8; 32]);
        let allowed = vec![*other_key.verifying_key().as_bytes()];

        assert!(verify_oracle_signed_maa_attestation_v1(&att, &allowed).is_err());
    }
}

impl<S: Spec, T> SequencerRemuneration<S> for StandardProvenRollupCapabilities<'_, S, T> {
    fn reward_sequencer_or_refund<
        Accessor: StateReader<Kernel, Error = Infallible>
            + StateWriter<Kernel, Error = Infallible>
            + StateWriter<User, Error = Infallible>
            + StateReader<User, Error = Infallible>,
    >(
        &mut self,
        sequencer: &<S::Da as DaSpec>::Address,
        sequencer_rollup_address: &S::Address,
        reward: SequencerReward,
        state: &mut Accessor,
    ) {
        let stake_increased = self.sequencer_registry.add_to_stake(
            self.bank.id().to_payable(),
            sequencer,
            reward.0,
            state,
        );

        // The error indicates that the forced registration was reverted.
        // In this case, we will refund the rewards to the user.
        if stake_increased.is_err() {
            self.bank
                .transfer_from(
                    self.bank.id.clone().to_payable(),
                    sequencer_rollup_address.as_token_holder(),
                    gas_coins(reward.0),
                    state,
                )
                // SAFETY: It is safe to unwrap here because the caller must ensure that sufficient funds are reserved.
                .expect("Caller failed to ensure sufficient funds are reserved. Transferring the consumed base fee gas is infallible");
        }
    }

    fn preferred_sequencer(
        &self,
        scratchpad: &mut impl InfallibleStateAccessor,
    ) -> Option<<S::Da as DaSpec>::Address> {
        self.sequencer_registry.preferred_sequencer(scratchpad)
    }
}
