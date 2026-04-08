use std::path::PathBuf;
use std::{env, num::NonZero};

use backon::{BackoffBuilder, ExponentialBuilder};
use sha2::{Digest, Sha256};
use sov_midnight_adapter::MidnightIndexerClient;
use sov_rollup_interface::da::BlockHeaderTrait;
use sov_rollup_interface::node::da::DaService;
use sov_rollup_interface::node::{future_or_shutdown, FutureOrShutdownOutput};
use sov_rollup_interface::stf::ProofSender;
use sov_rollup_interface::zk::aggregated_proof::SerializedAggregatedProof;
use tee::common::{BatchPublicDataV1, Engine};
use tee::maa::*;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use tracing::warn;
use types::{BlockProofInfo, BlockProofStatus, UnAggregatedProofList};

use self::types::AggregateProofMetadata;
use super::StateTransitionInfo;
use crate::processes::executor_client::{batch_public_data_to_executor_json, ExecutorClient};
use crate::processes::tee_manager::types::merkle_root_from_leaves;
use crate::processes::{hash_to_bytes32, ProverService, PublicDataTee, Receiver};
use tracing::info;

mod types;

const BACKOFF_POLICY_MIN_DELAY: u64 = 1;
const BACKOFF_POLICY_MAX_DELAY: u64 = 60;
const BACKOFF_POLICY_MAX_NUM_RETRIES: usize = 5;

fn env_flag_enabled(var_name: &str) -> bool {
    let Ok(raw) = env::var(var_name) else {
        return false;
    };

    let v = raw.trim();
    if v.is_empty() {
        return true;
    }

    match v.to_ascii_lowercase().as_str() {
        "0" | "false" | "no" | "off" => false,
        _ => true,
    }
}

pub(crate) fn compute_batch_hash_v1(
    batch_version: u8,
    batch_index: u64,
    parent_batch_hash: &[u8; 32],
    da_commitment: &[u8; 32],
    da_start_height: u64,
    da_end_height: u64,
) -> [u8; 32] {
    // Domain tag
    const TAG: &[u8] = b"BATCH_V1";

    // Forcing little-endian encoding for consistency
    let batch_index_le = batch_index.to_le_bytes();
    let da_start_le = da_start_height.to_le_bytes();
    let da_end_le = da_end_height.to_le_bytes();

    // version is u8, 1 byte fixed-width
    let ver = [batch_version];

    let h = midnight_privacy::poseidon2_hash(
        TAG,
        &[
            &ver,              // 1
            &batch_index_le,   // 8
            parent_batch_hash, // 32
            da_commitment,     // 32
            &da_start_le,      // 8
            &da_end_le,        // 8
        ],
    );

    h
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
    stf_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
    shutdown_receiver: tokio::sync::watch::Receiver<()>,
    http_client: reqwest::Client,
    oracle_url: String,
    midnight_bridge: Option<MidnightIndexerClient>,
    executor_client: Option<ExecutorClient>,
    rollup_id: Option<[u8; 32]>,
    storage_path: Option<PathBuf>,
    /// Rollup REST base URL for querying STF module state (e.g. withdraw_root).
    rollup_url: Option<String>,
    /// Consecutive L1 settlement failure count (for exponential backoff).
    l1_settlement_failures: u32,
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
        stf_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
        shutdown_receiver: tokio::sync::watch::Receiver<()>,
        http_client: reqwest::Client,
        oracle_url: String,
        midnight_bridge: Option<MidnightIndexerClient>,
        executor_client: Option<ExecutorClient>,
        rollup_id: Option<[u8; 32]>,
        storage_path: Option<PathBuf>,
        rollup_url: Option<String>,
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
            http_client,
            oracle_url,
            midnight_bridge,
            executor_client,
            rollup_id,
            storage_path,
            rollup_url,
            l1_settlement_failures: 0,
        }
    }

    async fn create_aggregate_proof_with_retries(
        &self,
        mut metadata: AggregateProofMetadata<Ps>,
        prover_service: &Ps,
        midnight_bridge: &Option<MidnightIndexerClient>,
        genesis_state_root: &Ps::StateRoot,
    ) -> anyhow::Result<(SerializedAggregatedProof, PublicDataTee)> {
        let mut attempt_num = 1u32;
        let mut backoff_iter = self.backoff_policy.build();

        loop {
            let maybe_backoff_duration = backoff_iter.next();

            match metadata
                .prove(prover_service, midnight_bridge, genesis_state_root)
                .await
            {
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
                        slot_number = stf_info.slot_number.get(),
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

            tracing::debug!(
                "Adding block at slot number {} (block hash {}) to proofs_to_create",
                slot_number,
                block_hash
            );

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
            tracing::debug!("Aggregating...");
            self.proofs_to_create.close_newest_proof();
            let metadata = self.proofs_to_create.take_oldest();

            let infos = &metadata.block_proof_info;

            // Get DA heights from cache (min/max keys give us start/end heights)
            let da_start_height = infos.iter().map(|b| b.da_height).min().unwrap();
            let da_end_height = infos.iter().map(|b| b.da_height).max().unwrap();

            let da_leaves: Vec<[u8; 32]> = infos
                .iter()
                .map(|b| hash_to_bytes32(&b.hash).expect("Block hash should be 32 bytes"))
                .collect();

            let da_commitment_root = merkle_root_from_leaves(da_leaves);
            tracing::debug!("DA commitment root: {:?}", da_commitment_root);

            tracing::info!(
                "Creating aggregated proof for blocks covering DA heights {} to {}",
                da_start_height,
                da_end_height
            );

            let (agg_proof, public_data) = self
                .create_aggregate_proof_with_retries(
                    metadata,
                    prover_service,
                    &self.midnight_bridge,
                    &self.genesis_state_root,
                )
                .await?;

            // Compute batch hash
            let batch_hash = compute_batch_hash_v1(
                1,
                self.batch_index,
                &self.prev_batch_hash,
                &da_commitment_root,
                da_start_height,
                da_end_height,
            );

            // Build the batch struct early so we can persist it for crash recovery.
            // NOTE: withdraw_root is now sourced from the L2 STF (via the prover's
            // REST query to the rollup), not the L1 indexer snapshot.
            let batch = BatchPublicDataV1 {
                version: 1,
                layer2_chain_id: public_data.layer2_chain_id,
                batch_index: self.batch_index,
                da_start_height,
                da_end_height,
                da_commitment: da_commitment_root,
                prev_state_root: public_data.initial_state_root,
                post_state_root: public_data.final_state_root,
                prev_batch_hash: self.prev_batch_hash,
                batch_hash,
                last_processed_queue_index: public_data.last_processed_queue_index,
                message_queue_hash: public_data.message_queue_hash,
                withdraw_root: public_data.withdraw_root,
            };

            // Backoff: if previous settlement failed, wait before retrying.
            if self.l1_settlement_failures > 0 {
                let delay_secs = std::cmp::min(
                    2u64.saturating_pow(self.l1_settlement_failures),
                    60,
                );
                info!(
                    batch_index = self.batch_index,
                    failures = self.l1_settlement_failures,
                    delay_secs,
                    "Backing off before retrying L1 settlement"
                );
                sleep(Duration::from_secs(delay_secs)).await;
            }

            // Resync: query L1 state to detect if the contract is ahead of our
            // cursor (e.g. after crash recovery finalized a batch, or if the
            // executor returned an error but the L1 tx actually succeeded).
            let mut skip_settlement = false;
            let mut skip_commit = false;
            if let Some(ref executor) = self.executor_client {
                match executor.get_state().await {
                    Ok(state) => {
                        info!(
                            batch_index = self.batch_index,
                            l1_finalized = state.last_finalized_batch_index,
                            l1_committed = state.last_committed_batch_index,
                            prev_batch_hash = hex::encode(self.prev_batch_hash),
                            "L1 state before batch settlement"
                        );
                        if state.last_finalized_batch_index >= self.batch_index {
                            // L1 already finalized this batch (or later) — resync cursor.
                            info!(
                                batch_index = self.batch_index,
                                l1_finalized = state.last_finalized_batch_index,
                                "L1 is ahead; resyncing cursor"
                            );
                            self.batch_index = state.last_finalized_batch_index + 1;
                            self.prev_batch_hash = state.last_finalized_batch_hash;
                            if let Some(ref sp) = self.storage_path {
                                super::bridge_lifecycle::remove_pending_finalize(sp);
                            }
                            self.l1_settlement_failures = 0;
                            skip_settlement = true;
                        } else if state.last_committed_batch_index >= self.batch_index
                            && state.last_committed_batch_index > state.last_finalized_batch_index
                        {
                            // Batch already committed but not finalized — skip commit,
                            // proceed directly to finalize.
                            info!(
                                batch_index = self.batch_index,
                                "Batch already committed on L1; skipping commit, proceeding to finalize"
                            );
                            skip_commit = true;
                        } else if state.last_finalized_batch_hash != self.prev_batch_hash
                            && state.last_finalized_batch_index + 1 == self.batch_index
                        {
                            // Our prev_batch_hash doesn't match L1's — resync.
                            warn!(
                                batch_index = self.batch_index,
                                our_prev = hex::encode(self.prev_batch_hash),
                                l1_prev = hex::encode(state.last_finalized_batch_hash),
                                "prev_batch_hash mismatch; resyncing from L1"
                            );
                            self.prev_batch_hash = state.last_finalized_batch_hash;
                        }
                    }
                    Err(e) => {
                        warn!(
                            batch_index = self.batch_index,
                            error = %e,
                            "Failed to query executor state for resync; proceeding with cached cursor"
                        );
                    }
                }
            }

            // Commit batch on L1 via executor service (if configured).
            // Track success so we only advance the durable batch cursor when L1 accepted both commit AND finalize.
            let mut l1_ok = if skip_settlement {
                true // cursor already resynced above
            } else if skip_commit {
                true // commit already on L1, proceed to finalize
            } else if let Some(ref executor) = self.executor_client {
                match executor
                    .commit_batch(&self.prev_batch_hash, &batch_hash)
                    .await
                {
                    Ok(()) => {
                        info!(
                            batch_index = self.batch_index,
                            "L1 commitBatch submitted via executor"
                        );

                        // Persist batch data so we can finalize on restart if the
                        // process crashes between commit and finalize.
                        if let Some(ref sp) = self.storage_path {
                            if let Some(ref rid) = self.rollup_id {
                                if let Ok(bpd_json) = batch_public_data_to_executor_json(&batch, rid) {
                                    let pf = super::bridge_lifecycle::PendingFinalize {
                                        batch_index: self.batch_index,
                                        batch_public_data_json: bpd_json,
                                        rollup_id_hex: hex::encode(rid),
                                    };
                                    if let Err(e) = super::bridge_lifecycle::persist_pending_finalize(sp, &pf) {
                                        warn!(error = %e, "Failed to persist pending-finalize (non-fatal)");
                                    }
                                }
                            }
                        }

                        true
                    }
                    Err(e) => {
                        warn!(
                            batch_index = self.batch_index,
                            error = %e,
                            "Executor commit_batch failed; batch cursor will NOT advance"
                        );
                        false
                    }
                }
            } else {
                tracing::debug!(
                    batch_index = self.batch_index,
                    "Executor not configured; skipping L1 commit/finalize (configure [sequencer.extension.midnight_bridge] to enable)"
                );
                true // no executor configured, L1 interaction is optional
            };

            let mock_attestation = env_flag_enabled("SOV_TEE_MOCK_ATTESTATION");
            let skip_oracle = env_flag_enabled("SOV_TEE_SKIP_ORACLE");

            let attestation_jwt = if mock_attestation {
                tracing::warn!(
                    "SOV_TEE_MOCK_ATTESTATION is enabled: publishing a mock MAA attestation"
                );
                format!("mock-maa-jwt(batch_index={})", self.batch_index)
            } else {
                attest(&batch, "midnight-l2")? // Hardcoded for now, should be replaced.
            };

            if skip_oracle {
                anyhow::bail!(
                    "SOV_TEE_SKIP_ORACLE is enabled, but TEE mode now requires an oracle-signed receipt. \
                     Start the oracle service and (for local dev) set ORACLE_DEV_ACCEPT_ALL=1."
                );
            }

            let statement = sov_modules_api::TeeOracleStatementV1 {
                domain: sov_modules_api::TEE_ORACLE_STATEMENT_DOMAIN_V1,
                attestation_type: sov_modules_api::TEEAttestationType::MAA,
                batch_data: batch.clone(),
                raw_aggregated_proof_sha256: Sha256::digest(&agg_proof.raw_aggregated_proof).into(),
                attestation_jwt_sha256: Sha256::digest(attestation_jwt.as_bytes()).into(),
            };

            // Ask the oracle to (1) validate the TEE attestation and (2) sign the statement for deterministic on-chain verification.
            let oracle_req = sov_modules_api::OracleAttestRequestV1 {
                attestation_jwt: attestation_jwt.clone(),
                statement: statement.clone(),
            };
            let oracle_req_bytes = borsh::to_vec(&oracle_req)?;

            let oracle_res_http = self
                .http_client
                .post(format!("{}/attest", self.oracle_url))
                .json(&tee::common::TEEPayload {
                    data: tee::common::BASE64_ENGINE.encode(oracle_req_bytes),
                })
                .send()
                .await?;

            let status = oracle_res_http.status();
            if !status.is_success() {
                let body = oracle_res_http.text().await.unwrap_or_default();
                anyhow::bail!("Oracle /attest failed: {} {}", status, body);
            }

            let oracle_payload: tee::common::TEEPayload = oracle_res_http.json().await?;
            let oracle_res_bytes = tee::common::BASE64_ENGINE
                .decode(oracle_payload.data.trim())
                .map_err(|e| anyhow::anyhow!("Invalid oracle base64 response: {e}"))?;
            let oracle_res: sov_modules_api::OracleAttestResponseV1 =
                borsh::from_slice(&oracle_res_bytes)?;

            let signed_attestation = sov_modules_api::TeeOracleSignedMAAAttestationV1 {
                attestation_jwt,
                statement,
                oracle_pubkey: oracle_res.oracle_pubkey,
                oracle_signature: oracle_res.oracle_signature,
            };

            tracing::debug!(
                bytes = agg_proof.raw_aggregated_proof.len(),
                "Sending aggregated proof and attestation to DA (for now, to be replaced)"
            );

            let attestation = sov_modules_api::TEEAttestation {
                attestation: borsh::to_vec(&signed_attestation)?,
                raw_aggregated_proof: agg_proof.raw_aggregated_proof,
                batch_data: batch.clone(),
                attestation_type: sov_modules_api::TEEAttestationType::MAA,
            };

            let borshed_attestation = borsh::to_vec(&attestation)?;

            let attestation = sov_modules_api::SerializedTEEAttestation {
                tee_raw_attestation: borshed_attestation.clone(),
            };

            tracing::info!(
                batch_index = self.batch_index,
                bytes = borshed_attestation.len(),
                "Posting oracle-signed TEE attestation"
            );

            self.proof_sender
                .publish_tee_attestation_blob_with_metadata(attestation)
                .await?;

            // Finalize batch on L1 via executor service (if configured).
            // Only attempt finalize if commit succeeded -- otherwise the contract is not expecting it.
            // Skip if we already resynced the cursor above.
            if l1_ok && !skip_settlement {
                if let (Some(ref executor), Some(rollup_id)) =
                    (self.executor_client.as_ref(), self.rollup_id.as_ref())
                {
                    match batch_public_data_to_executor_json(&batch, rollup_id) {
                        Ok(batch_public_data_json) => {
                            const SIGNATURE_MAX_NONCE: u64 = 256;
                            const SIGNER_BITMAP: u8 = 0b111; // three signers
                            const FINALIZE_TIMESTAMP: u64 = 0;

                            match executor
                                .build_signatures(&batch_public_data_json, SIGNATURE_MAX_NONCE)
                                .await
                            {
                                Ok(signatures_json) => {
                                    if let Err(e) = executor
                                        .finalize_batch(
                                            &batch_public_data_json,
                                            &signatures_json,
                                            SIGNER_BITMAP,
                                            FINALIZE_TIMESTAMP,
                                        )
                                        .await
                                    {
                                        warn!(
                                            batch_index = self.batch_index,
                                            error = %e,
                                            "Executor finalize_batch failed; batch cursor will NOT advance"
                                        );
                                        l1_ok = false;
                                    } else {
                                        info!(
                                            batch_index = self.batch_index,
                                            "L1 finalizeBatch submitted via executor"
                                        );
                                        if let Some(ref sp) = self.storage_path {
                                            super::bridge_lifecycle::remove_pending_finalize(sp);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        batch_index = self.batch_index,
                                        error = %e,
                                        "Executor build_signatures failed; batch cursor will NOT advance"
                                    );
                                    l1_ok = false;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                batch_index = self.batch_index,
                                error = %e,
                                "Failed to serialize batch public data for executor; batch cursor will NOT advance"
                            );
                            l1_ok = false;
                        }
                    }
                }
            }

            // Update the next height to receive
            self.stf_info_receiver
                .inc_next_height_to_receive_by(num_proofs_to_create as u64);

            if l1_ok {
                if !skip_settlement {
                    self.batch_index += 1;
                    self.prev_batch_hash = batch_hash;
                }
                self.l1_settlement_failures = 0;
            } else {
                self.l1_settlement_failures = self.l1_settlement_failures.saturating_add(1);
                warn!(
                    batch_index = self.batch_index,
                    failures = self.l1_settlement_failures,
                    "L1 commit/finalize failed; batch cursor NOT advanced (will retry with backoff)"
                );
            }
        }
        tracing::debug!("Finished processing STF info");
        Ok(())
    }
}
