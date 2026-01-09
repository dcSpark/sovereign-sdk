use std::collections::HashMap;
use std::num::NonZero;

use backon::{BackoffBuilder, ExponentialBuilder};
use sov_rollup_interface::da::BlockHeaderTrait;
use sov_rollup_interface::node::da::DaService;
use sov_rollup_interface::node::{future_or_shutdown, FutureOrShutdownOutput};
use sov_rollup_interface::stf::ProofSender;
use sov_rollup_interface::zk::aggregated_proof::SerializedAggregatedProof;
use tee::common::BatchPublicDataV1;
use tee::maa::*;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use types::{BlockProofInfo, BlockProofStatus, UnAggregatedProofList};

use self::types::AggregateProofMetadata;
use super::StateTransitionInfo;
use crate::processes::tee_manager::types::merkle_root_from_leaves;
use crate::processes::{ProverService, PublicDataTee, Receiver};

mod types;

const BACKOFF_POLICY_MIN_DELAY: u64 = 1;
const BACKOFF_POLICY_MAX_DELAY: u64 = 60;
const BACKOFF_POLICY_MAX_NUM_RETRIES: usize = 5;

pub(crate) fn hash_to_bytes32<H: AsRef<[u8]>>(h: &H) -> anyhow::Result<[u8; 32]> {
    let b = h.as_ref();
    anyhow::ensure!(
        b.len() == 32,
        "hash_to_bytes32: expected 32 bytes, got {}",
        b.len()
    );
    Ok(b.try_into().unwrap())
}

pub(crate) fn state_root_to_bytes32<R: AsRef<[u8]>>(root: &R) -> anyhow::Result<[u8; 64]> {
    let b = root.as_ref();
    anyhow::ensure!(
        b.len() == 64,
        "state_root_to_bytes32: expected 64 bytes, got {}",
        b.len()
    );
    Ok(b.try_into().unwrap())
}

/// Manages the lifecycle of the `AggregatedProof`.
#[allow(clippy::type_complexity)]
#[allow(dead_code)]
pub struct TeeProofManager<Ps: ProverService> {
    prover_service: Ps,
    proofs_to_create: UnAggregatedProofList<Ps>,
    aggregated_proof_block_jump: NonZero<usize>,
    proof_sender: Box<dyn ProofSender>,
    backoff_policy: ExponentialBuilder,
    genesis_state_root: Ps::StateRoot,
    batch_index: u64,
    prev_batch_hash: [u8; 32],
    layer2_chain_id: u64,
    stf_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
    shutdown_receiver: tokio::sync::watch::Receiver<()>,
    // Store DA heights indexed by slot number for later retrieval
    da_height_cache: HashMap<u64, u64>,
}

impl<Ps: ProverService> TeeProofManager<Ps>
where
    Ps::DaService: DaService<Error = anyhow::Error>,
{
    /// Creates a new proof manager.
    #[allow(clippy::type_complexity)]
    pub fn new(
        prover_service: Ps,
        aggregated_proof_block_jump: NonZero<usize>,
        proof_sender: Box<dyn ProofSender>,
        genesis_state_root: Ps::StateRoot,
        batch_index: u64,
        prev_batch_hash: [u8; 32],
        layer2_chain_id: u64,
        stf_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
        shutdown_receiver: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self {
            prover_service,
            proofs_to_create: UnAggregatedProofList::new(),
            aggregated_proof_block_jump,
            proof_sender,
            backoff_policy: ExponentialBuilder::default()
                .with_min_delay(Duration::from_secs(BACKOFF_POLICY_MIN_DELAY))
                .with_max_delay(Duration::from_secs(BACKOFF_POLICY_MAX_DELAY))
                .with_max_times(BACKOFF_POLICY_MAX_NUM_RETRIES),
            genesis_state_root,
            stf_info_receiver,
            shutdown_receiver,
            batch_index,
            prev_batch_hash,
            layer2_chain_id,
            da_height_cache: HashMap::new(),
        }
    }

    async fn create_aggregate_proof_with_retries(
        &self,
        mut metadata: AggregateProofMetadata<Ps>,
        prover_service: &Ps,
        genesis_state_root: &Ps::StateRoot,
    ) -> anyhow::Result<(SerializedAggregatedProof, PublicDataTee)> {
        let mut attempt_num = 1u32;
        let mut backoff_iter = self.backoff_policy.build();

        loop {
            let maybe_backoff_duration = backoff_iter.next();

            match metadata.prove(prover_service, genesis_state_root).await {
                Ok((proof, public_data)) => return Ok((proof, public_data)),
                Err((returned_metadata, error)) => {
                    let error_message = format!("Failed to generate aggregate proof: {error}");

                    if error_message.contains("Elf parse error") {
                        // NOTE We exit early on this error since it means the we've failed to find/parse
                        // the zk circuit, and there's no recovering from that.
                        tracing::error!("Fatal error: {error_message}");
                        tracing::error!(
                            "Please check your zk circuit ELF file was built correctly!"
                        );
                        anyhow::bail!(error)
                    };

                    tracing::error!(error_message);
                    match maybe_backoff_duration {
                        None => {
                            tracing::warn!("Maximum number of retries exhausted - exiting");
                            anyhow::bail!(error)
                        }
                        Some(duration) => {
                            tracing::info!("Retrying generation of aggregate proof in {}s, attempt {attempt_num} of {}...", duration.as_secs(), BACKOFF_POLICY_MAX_NUM_RETRIES);
                            attempt_num += 1;
                            sleep(duration).await;
                            metadata = returned_metadata;
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Starts a background task for `AggregatedProof` generation.
    pub async fn post_aggregated_proof_to_da_in_background(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("Spawning an aggregated proof posting background task");
            if let Err(e) = self.post_aggregated_proof_to_da_when_ready().await {
                tracing::error!(error = ?e, "Failed to post aggregated proof to DA");
            }
        })
    }

    /// Attempts to generate an `AggregatedProof` and then posts it to DA.
    /// The proof is created only when there are enough of inner proofs in the `ProverService` queue.
    async fn post_aggregated_proof_to_da_when_ready(mut self) -> anyhow::Result<()> {
        loop {
            match future_or_shutdown(self.stf_info_receiver.read_next(), &self.shutdown_receiver)
                .await
            {
                FutureOrShutdownOutput::Shutdown => {
                    tracing::info!("Shutting down aggregated proof posting task...");
                    break;
                }
                FutureOrShutdownOutput::Output(stf_info_result) => {
                    let stf_info = match stf_info_result? {
                        None => {
                            tracing::debug!("Received None instead of StateTransitionInfo. This can happen if the transition has already been processed by the `Receiver`. In that case, it is fine to ignore the notification.");
                            continue;
                        }
                        Some(stf_info) => stf_info,
                    };
                    tracing::trace!(
                        slot_number = %stf_info.slot_number,
                        block_header = %stf_info.da_block_header().display(),
                        "Received STF info");

                    self.process_stf_info(stf_info).await?;
                }
            }
        }
        tracing::debug!("Aggregated proofs posting task has been completed");
        Ok(())
    }

    /// Processes current STF info and optionally published aggregated proof to DA.
    async fn process_stf_info(
        &mut self,
        stf_info: StateTransitionInfo<
            Ps::StateRoot,
            Ps::Witness,
            <Ps::DaService as DaService>::Spec,
        >,
    ) -> anyhow::Result<()> {
        let first_height_unproven = self.stf_info_receiver.next_height_to_receive();

        let prover_service = &self.prover_service;

        // We ensure that we're not trying to prove blocks that are being proven.
        // If that is not the case, we add the block to the queue.
        if first_height_unproven.saturating_add(self.proofs_to_create.current_proof_jump() as u64)
            <= stf_info.slot_number
        {
            let block_hash = stf_info.da_block_header().hash();
            let slot_number = stf_info.slot_number.get();
            let da_height = stf_info.da_block_header().height();

            println!(
                "Adding block at slot number {} to proofs_to_create",
                slot_number
            );

            println!("{}", block_hash);

            // Save the transition for later proving. This is temporarily redundant
            // since we always just try to prove blocks right away (because we don't have fee
            // estimates for proving built out yet).
            self.proofs_to_create.append(BlockProofInfo {
                status: BlockProofStatus::Waiting(stf_info),
                hash: block_hash,
                // TODO(@preston-evans98): estimate public data size. This requires a new API on the `prover_service`.
                // <https://github.com/Sovereign-Labs/sovereign-sdk-wip/issues/440>
                public_data_size: 0,
                da_height,
            });
        }

        // Start proving the next block right away... for now.
        self.proofs_to_create
            .oldest_mut()
            .prove_any_unproven_blocks(prover_service)
            .await;

        let num_proofs_to_create = self.proofs_to_create.current_proof_jump();

        // If we've covered enough blocks for the aggregate proof, generate and submit it to DA
        if num_proofs_to_create >= self.aggregated_proof_block_jump.get() {
            println!("Aggregating...");
            self.proofs_to_create.close_newest_proof();
            let metadata = self.proofs_to_create.take_oldest();

            let infos = &metadata.block_proof_info;

            // Get DA heights from cache (min/max keys give us start/end heights)
            let da_start_height = infos.iter().map(|b| b.da_height).min().unwrap();
            let da_end_height = infos.iter().map(|b| b.da_height).max().unwrap();

            let da_leaves: Vec<[u8; 32]> = infos
                .iter()
                .map(|b| {
                    borsh::to_vec(&b.hash)
                        .expect("Serialization should succeed")
                        .try_into()
                        .expect("Serialization should produce 32 bytes")
                })
                .collect();

            let da_commitment_root = merkle_root_from_leaves(da_leaves);
            println!("DA commitment root: {:?}", da_commitment_root);

            println!(
                "Creating aggregated proof for blocks covering DA heights {} to {}",
                da_start_height, da_end_height
            );

            let (agg_proof, public_data) = self
                .create_aggregate_proof_with_retries(
                    metadata,
                    prover_service,
                    &self.genesis_state_root,
                )
                .await?;

            // Compute batch hash
            // This is NOT correct, it's a placeholder until we have the correct way to compute it
            let batch_hash = hash_to_bytes32(&public_data.final_slot_hash)?;

            tracing::debug!("Generating TEE attestation...");

            let prev_state_root = public_data.initial_state_root;
            let post_state_root = public_data.final_state_root;
            let slot_hash = public_data.final_slot_hash;
            let withdraw_root = public_data.withdraw_root;
            let message_queue_hash = public_data.message_queue_hash;
            let last_processed_queue_index = public_data.last_processed_queue_index;

            //daCommitment is not computed yet
            let batch = BatchPublicDataV1 {
                version: 1,
                layer2_chain_id: self.layer2_chain_id,
                batch_index: self.batch_index,
                da_start_height,
                da_end_height,
                da_commitment: da_commitment_root,
                prev_state_root,
                post_state_root,
                prev_batch_hash: self.prev_batch_hash,
                batch_hash,
                last_processed_queue_index,
                message_queue_hash,
                withdraw_root,
            };

            let attestation = attest(&batch, "midnight-l2")?; // Hardcoded for now, should be replaced.

            tracing::debug!(
                bytes = agg_proof.raw_aggregated_proof.len(),
                "Sending aggregated proof and attestation to DA (for now, to be replaced)"
            );

            let attestation = sov_modules_api::TEEAttestation {
                attestation: borsh::to_vec(&attestation)?,
                raw_aggregated_proof: agg_proof.raw_aggregated_proof,
                batch_data: batch,
                attestation_type: sov_modules_api::TEEAttestationType::MAA,
            };

            println!(
                "Posting TEE attestation for batch index {}",
                self.batch_index
            );
            println!("TEE attestation: {:?}", attestation);

            let attestation = sov_modules_api::SerializedTEEAttestation {
                tee_raw_attestation: borsh::to_vec(&attestation)?,
            };

            println!(
                "Serialized TEE attestation size: {}",
                borsh::to_vec(&attestation)?.len()
            );
            println!("Sending aggregated proof and attestation to DA (for now, to be replaced)");

            self.proof_sender
                .publish_tee_attestation_blob_with_metadata(attestation)
                .await?;

            // Update the next height to receive
            self.stf_info_receiver
                .inc_next_height_to_receive_by(num_proofs_to_create as u64);

            self.batch_index += 1;

            self.prev_batch_hash = batch_hash;
        }
        println!("Done");
        Ok(())
    }
}
