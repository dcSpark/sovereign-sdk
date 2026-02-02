use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use alloy_primitives::U256;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sov_midnight_adapter::MidnightIndexerClient;
use sov_rollup_interface::da::{BlockHeaderTrait, DaSpec, DaVerifier};
use sov_rollup_interface::node::da::DaService;
use sov_rollup_interface::zk::aggregated_proof::{
    AggregatedProofPublicData, CodeCommitment, SerializedAggregatedProof,
};
use sov_rollup_interface::zk::{
    StateTransitionPublicData, StateTransitionWitness, StateTransitionWitnessWithAddress, Zkvm,
    ZkvmHost,
};
use tracing::{error, info, trace};

use super::state::{ProverState, ProverStatus};
use super::{ProverServiceError, Verifier};
use crate::processes::prover_service::block_proof::BlockProof;
use crate::processes::{
    hash_to_bytes32, state_root_to_bytes32, ProofAggregationStatus, ProofProcessingStatus,
    PublicDataTee, RollupProverConfigDiscriminants, StateTransitionInfo,
};

#[derive(Clone, Default, BorshSerialize, BorshDeserialize)]
pub(crate) struct L1BridgeData {
    pub withdraw_root: [u8; 32],
    pub message_queue_hash: [u8; 32],
    pub last_processed_queue_index: U256,
    pub layer2_chain_id: u64,
}

fn default_l1_bridge_cache_filename() -> &'static str {
    "tee_l1_bridge_cache.borsh"
}

fn load_l1_bridge_cache(path: &Path) -> Option<L1BridgeData> {
    let bytes = std::fs::read(path).ok()?;
    borsh::from_slice(&bytes).ok()
}

fn store_l1_bridge_cache(path: &Path, value: &L1BridgeData) -> anyhow::Result<()> {
    let Some(parent) = path.parent() else {
        anyhow::bail!("Invalid cache path (no parent directory): {}", path.display());
    };
    std::fs::create_dir_all(parent)?;

    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, borsh::to_vec(value)?)?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

// A prover that generates proofs in parallel using a thread pool. If the pool is saturated,
// the prover will reject new jobs.
pub(crate) struct Prover<Address, StateRoot, Witness, Da: DaService> {
    prover_address: Address,
    prover_state: Arc<RwLock<ProverState<Address, StateRoot, Da::Spec>>>,
    num_threads: usize,
    // From Docs:
    // """
    // When the ThreadPool is dropped,
    // that's a signal for the threads it manages to terminate,
    // they will complete executing any remaining work that you have spawned,
    // and automatically terminate.
    // """
    pool: rayon::ThreadPool,
    code_commitment: CodeCommitment,
    l1_bridge_cache_path: Option<PathBuf>,
    l1_bridge_cached: Arc<RwLock<L1BridgeData>>,
    warned_no_midnight_bridge: AtomicBool,
    phantom: std::marker::PhantomData<(StateRoot, Witness, Da)>,
}

impl<Address, StateRoot, Witness, Da> Prover<Address, StateRoot, Witness, Da>
where
    Da: DaService,
    Address:
        BorshSerialize + Serialize + DeserializeOwned + AsRef<[u8]> + Clone + Send + Sync + 'static,
    StateRoot: Serialize + DeserializeOwned + Clone + AsRef<[u8]> + Send + Sync + 'static,
    Witness: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub(crate) fn new(
        prover_address: Address,
        num_threads: usize,
        code_commitment: CodeCommitment,
        storage_path: Option<PathBuf>,
    ) -> Self {
        let l1_bridge_cache_path =
            storage_path.as_ref().map(|p| p.join(default_l1_bridge_cache_filename()));

        let cached = l1_bridge_cache_path
            .as_deref()
            .and_then(load_l1_bridge_cache)
            .unwrap_or_default();

        Self {
            code_commitment,
            num_threads,
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(num_threads)
                .build()
                .unwrap(),

            prover_state: Arc::new(RwLock::new(ProverState {
                prover_status: Default::default(),
                pending_tasks_count: Default::default(),
            })),
            prover_address,
            l1_bridge_cache_path,
            l1_bridge_cached: Arc::new(RwLock::new(cached)),
            warned_no_midnight_bridge: AtomicBool::new(false),
            phantom: PhantomData,
        }
    }

    pub(crate) fn start_proving<InnerVm>(
        &self,
        state_transition_info: StateTransitionInfo<StateRoot, Witness, <Da as DaService>::Spec>,
        config: RollupProverConfigDiscriminants,
        mut inner_vm: InnerVm::Host,
        verifier: Arc<Verifier<Da>>,
    ) -> Result<
        ProofProcessingStatus<StateRoot, Witness, <Da as DaService>::Spec>,
        ProverServiceError,
    >
    where
        InnerVm: Zkvm + 'static,
    {
        let block_header_hash = state_transition_info.da_block_header().hash();

        let mut prover_state = self.prover_state.write().expect("Lock was poisoned");
        if let Some(duplicate_proof) = prover_state.get_prover_status(&block_header_hash) {
            return match duplicate_proof {
                ProverStatus::ProvingInProgress => Err(anyhow::anyhow!(
                    "Proof generation for {} still in progress",
                    block_header_hash
                )
                .into()),
                ProverStatus::Proved(_) => Err(anyhow::anyhow!(
                    "Witness for block_header_hash {}, submitted multiple times.",
                    block_header_hash,
                )
                .into()),
                ProverStatus::Err(e) => Err(anyhow::format_err!("{}", e).into()), // "Clone" the anyhow error without cloning, because anyhow doesn't support that
            };
        }

        let start_prover = prover_state.inc_task_count_if_not_busy(self.num_threads);

        let prover_state_clone = self.prover_state.clone();
        // Initiate a new proving job only if the prover is not busy.
        if start_prover {
            prover_state.set_to_proving(block_header_hash.clone());

            let data = StateTransitionWitnessWithAddress {
                stf_witness: state_transition_info.data,
                prover_address: self.prover_address.clone(),
            };

            let prover_address = self.prover_address.clone();

            inner_vm.add_hint(&data);

            self.pool.spawn(move || {
                tracing::info_span!("guest_execution").in_scope(|| {
                    let proof = make_inner_proof::<InnerVm>(inner_vm, config);

                    let mut prover_state = prover_state_clone.write().expect("Lock was poisoned");

                    let StateTransitionWitness {
                        initial_state_root,
                        final_state_root,
                        da_block_header,
                        relevant_proofs,
                        relevant_blobs: blobs,
                        ..
                    } = data.stf_witness;

                    verifier
                        .da_verifier
                        .verify_relevant_tx_list(&da_block_header, &blobs, relevant_proofs)
                        .expect("An honest prover provided an invalid list of relevant txs. This is a bug in the prover - please report it.");

                    let block_proof = proof.map(|p| BlockProof {
                        _proof: p,
                        st: StateTransitionPublicData::<Address, Da::Spec, StateRoot> {
                            initial_state_root,
                            final_state_root,
                            slot_hash: block_header_hash.clone(),
                            prover_address,
                        },
                        slot_number: state_transition_info.slot_number,
                    });

                    prover_state.set_to_proved(block_header_hash, block_proof);
                    prover_state.dec_task_count();
                });
            });

            Ok(ProofProcessingStatus::ProvingInProgress)
        } else {
            Ok(ProofProcessingStatus::Busy(state_transition_info))
        }
    }

    pub(crate) fn create_aggregated_proof<OuterVm: ZkvmHost + 'static>(
        &self,
        mut outer_vm: OuterVm,
        block_header_hashes: &[<Da::Spec as DaSpec>::SlotHash],
        midnight_bridge: &Option<MidnightIndexerClient>,
        genesis_state_root: &StateRoot,
    ) -> anyhow::Result<ProofAggregationStatus> {
        assert!(!block_header_hashes.is_empty());
        let mut prover_state = self.prover_state.write().expect("Lock was poisoned");

        let mut block_proofs_data = Vec::default();

        for slot_hash in block_header_hashes {
            let state = prover_state.get_prover_status(slot_hash);

            match state {
                Some(ProverStatus::ProvingInProgress) => {
                    return Ok(ProofAggregationStatus::ProofGenerationInProgress);
                }
                Some(ProverStatus::Proved(block_proof)) => {
                    assert_eq!(slot_hash, &block_proof.st.slot_hash);
                    block_proofs_data.push(block_proof);
                }
                Some(ProverStatus::Err(e)) => return Err(anyhow::anyhow!(e.to_string())),
                None => return Err(anyhow::anyhow!("Missing required proof of {:?}. Use the `prove` method to generate a proof of that block and try again.", slot_hash)),
            }
        }

        // It is ok to unwrap here as we asserted that block_proofs_data.len() >= 1.
        let initial_block_proof = block_proofs_data.first().unwrap();
        let final_block_proof = block_proofs_data.last().unwrap();

        let mut rewarded_addresses = Vec::default();
        for bp in block_proofs_data.iter() {
            rewarded_addresses.push(bp.st.prover_address.clone());
        }

        // Mainly here to avoid to init the midnight bridge if not needed.
        let mut l1_bridge = self
            .l1_bridge_cached
            .read()
            .expect("Lock was poisoned")
            .clone();

        if let Some(midnight_bridge) = midnight_bridge {
            let snap = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { midnight_bridge.snapshot().await })
            });

            match snap {
                Err(e) => {
                    tracing::error!(error = ?e, "Failed to get midnight bridge snapshot, L1 bridge data will be mocked values.");
                }
                Ok(snap) => {
                    let candidate_last_processed =
                        U256::from(snap.l2_messenger.last_processed_l1_index);
                    if candidate_last_processed < l1_bridge.last_processed_queue_index {
                        tracing::warn!(
                            previous = %l1_bridge.last_processed_queue_index,
                            candidate = %candidate_last_processed,
                            "Midnight bridge snapshot last_processed_l1_index regressed; keeping cached value"
                        );
                    } else {
                        l1_bridge.last_processed_queue_index = candidate_last_processed;
                    }

                    l1_bridge.layer2_chain_id = snap.rollup.layer2_chain_id;

                    let index = snap.rollup.next_cross_domain_message_index.saturating_sub(1);
                    if let Some(h) = snap.rollup.message_rolling_hashes.get(&index) {
                        l1_bridge.message_queue_hash = *h;
                    } else {
                        tracing::warn!(
                            next_cross_domain_message_index = snap.rollup.next_cross_domain_message_index,
                            "Missing message rolling hash for expected index; keeping cached value"
                        );
                    }

                    if let Some(root) = snap.rollup.withdraw_roots.values().next_back() {
                        l1_bridge.withdraw_root = *root;
                    } else {
                        tracing::warn!("No withdraw roots available in snapshot; keeping cached value");
                    }

                    // Persist best-effort so we don't reset to zeros on restart.
                    if let Some(path) = &self.l1_bridge_cache_path {
                        if let Err(err) = store_l1_bridge_cache(path, &l1_bridge) {
                            tracing::warn!(
                                error = ?err,
                                path = %path.display(),
                                "Failed to persist L1 bridge cache"
                            );
                        }
                    }

                    *self
                        .l1_bridge_cached
                        .write()
                        .expect("Lock was poisoned") = l1_bridge.clone();
                }
            }
        } else {
            if !self.warned_no_midnight_bridge.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "No midnight bridge configured; using cached/mock L1 bridge values for TEE batch public data"
                );
            }
        }

        let public_data = AggregatedProofPublicData::<Address, Da::Spec, StateRoot> {
            rewarded_addresses,
            initial_slot_number: initial_block_proof.slot_number,
            final_slot_number: final_block_proof.slot_number,
            genesis_state_root: genesis_state_root.clone(),
            initial_state_root: initial_block_proof.st.initial_state_root.clone(),
            final_state_root: final_block_proof.st.final_state_root.clone(),
            initial_slot_hash: initial_block_proof.st.slot_hash.clone(),
            final_slot_hash: final_block_proof.st.slot_hash.clone(),
            code_commitment: self.code_commitment.clone(),
            withdraw_root: l1_bridge.withdraw_root,
            message_queue_hash: l1_bridge.message_queue_hash,
        };

        let public_tee: PublicDataTee = PublicDataTee {
            initial_state_root: state_root_to_bytes32(&public_data.initial_state_root)?,
            final_state_root: state_root_to_bytes32(&public_data.final_state_root)?,
            final_slot_hash: hash_to_bytes32(&public_data.final_slot_hash)?,
            withdraw_root: public_data.withdraw_root,
            message_queue_hash: public_data.message_queue_hash,
            last_processed_queue_index: l1_bridge.last_processed_queue_index,
            layer2_chain_id: l1_bridge.layer2_chain_id,
        };

        trace!(%public_data, "generating aggregate proof");
        // TODO: https://github.com/Sovereign-Labs/sovereign-sdk-wip/issues/316
        // `add_hint`  should take witness instead of the public input.
        outer_vm.add_hint(public_data);
        let serialized_aggregated_proof = SerializedAggregatedProof {
            raw_aggregated_proof: outer_vm.run(false)?,
        };

        for slot_hash in block_header_hashes {
            prover_state.remove(slot_hash);
        }
        Ok(ProofAggregationStatus::Success(
            serialized_aggregated_proof,
            public_tee,
        ))
    }
}

fn make_inner_proof<InnerVm>(
    mut vm: InnerVm::Host,
    config: RollupProverConfigDiscriminants,
) -> anyhow::Result<Vec<u8>>
where
    InnerVm: Zkvm + 'static,
{
    let proving_start = std::time::Instant::now();
    let result = match config {
        RollupProverConfigDiscriminants::Skip => Ok(Vec::default()),
        RollupProverConfigDiscriminants::Execute => {
            info!(
                "Executing in VM without constructing proof using {}",
                std::any::type_name::<InnerVm>()
            );
            vm.run(false)
        }
        RollupProverConfigDiscriminants::Prove => {
            info!("Generating proof with {}", std::any::type_name::<InnerVm>());
            vm.run(true)
        }
    };
    sov_metrics::track_metrics(|tracker| {
        let proving_time = proving_start.elapsed();
        let is_success = result.is_ok();
        tracker.submit(sov_metrics::ZkProvingTime {
            proving_time,
            is_success,
            zk_circuit: sov_metrics::ZkCircuit::Inner,
        });
    });
    match result {
        Ok(ref proof) => {
            trace!(
                bytes = proof.len(),
                "Proof generation completed successfully"
            );
        }
        Err(ref e) => {
            error!("Proof generation failed: {:?}", e);
        }
    }
    result
}
