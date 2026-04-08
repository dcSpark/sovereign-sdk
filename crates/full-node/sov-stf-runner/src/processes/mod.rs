//! Processes responsible for creating different kind of proofs.
mod op_manager;
mod prover_service;
mod stf_info_manager;
mod zk_manager;
use std::num::NonZero;

pub mod bridge_lifecycle;
mod executor_client;
#[cfg(feature = "tee")]
mod tee_manager;

pub use executor_client::ExecutorClient;
#[cfg(feature = "tee")]
pub use executor_client::batch_public_data_to_executor_json;
#[cfg(feature = "tee")]
pub use tee_manager::*;

use op_manager::attestations::AttestationsManager;
pub use prover_service::*;
#[cfg(feature = "tee")]
use sov_midnight_adapter::MidnightIndexerClient;
use sov_rollup_interface::node::da::DaService;
use sov_rollup_interface::optimistic::BondingProofService;
use sov_rollup_interface::stf::ProofSender;
pub use stf_info_manager::*;
use tokio::sync::watch;
use tokio::task::JoinHandle;
#[cfg(feature = "tee")]
use tracing::{info, warn};
pub use zk_manager::*;

#[cfg(feature = "tee")]
/// Starts a process that generates aggregated proofs in the background.
pub async fn start_tee_workflow_in_background<Ps>(
    prover_service: Ps,
    aggregated_proof_block_jump: NonZero<usize>,
    proof_sender: Box<dyn ProofSender>,
    genesis_state_root: Ps::StateRoot,
    stf_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
    shutdown_receiver: tokio::sync::watch::Receiver<()>,
    oracle_url: String,
    midnight_bridge: Option<MidnightIndexerClient>,
    executor_client: Option<ExecutorClient>,
    rollup_id: Option<[u8; 32]>,
    storage_path: Option<std::path::PathBuf>,
    rollup_url: Option<String>,
) -> anyhow::Result<JoinHandle<()>>
where
    Ps: ProverService,
    Ps::DaService: DaService<Error = anyhow::Error>,
{
    // ---- Crash recovery: complete a committed-but-not-finalized batch from a
    //      previous run before anything else.
    if let (Some(ref sp), Some(ref executor)) = (&storage_path, &executor_client) {
        match bridge_lifecycle::recover_pending_finalize(sp, executor).await {
            Ok(true) => info!("Pending-finalize recovery succeeded"),
            Ok(false) => {}
            Err(e) => warn!(error = %e, "Pending-finalize recovery failed (will proceed; may hit COMMIT_OUT_OF_ORDER)"),
        }
    }

    let mut batch_data = 0u64;
    let mut prev_batch_hash = [0u8; 32];
    let mut seeded_from_l1 = false;

    // Prefer executor for seeding when present so the rollup's next commit parent
    // matches what the executor (and L1 contract) expect, avoiding BAD_PARENT_BATCH_HASH
    // when indexer and executor state differ (e.g. undeployed network or clean restart).
    if let Some(ref executor) = executor_client {
        match executor.get_state().await {
            Ok(state) => {
                let cursor = std::cmp::max(
                    state.last_committed_batch_index,
                    state.last_finalized_batch_index,
                );
                prev_batch_hash = if cursor == state.last_committed_batch_index
                    && state.last_committed_batch_index > state.last_finalized_batch_index
                {
                    state.last_committed_batch_hash
                } else {
                    state.last_finalized_batch_hash
                };
                batch_data = cursor + 1;
                seeded_from_l1 = true;
                info!(
                    last_finalized_batch_index = state.last_finalized_batch_index,
                    last_committed_batch_index = state.last_committed_batch_index,
                    next_batch_index = batch_data,
                    prev_batch_hash = hex::encode(prev_batch_hash),
                    "Seeded TEE batch cursor from executor /state"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "Executor /state unavailable at startup"
                );
            }
        }
    }

    // Fallback: seed from L1 Bridge adapter (indexer) when executor did not provide state.
    if !seeded_from_l1 {
        if let Some(client) = midnight_bridge.as_ref() {
            match client.snapshot().await {
                Ok(snap) => {
                    let last_finalized = snap.rollup.misc_data.last_finalized_batch_index;
                    batch_data = last_finalized + 1;
                    prev_batch_hash = snap.rollup.last_finalized_batch_hash;
                    seeded_from_l1 = true;
                    info!(
                        last_finalized_batch_index = last_finalized,
                        next_batch_index = batch_data,
                        prev_batch_hash = hex::encode(prev_batch_hash),
                        "Seeded TEE batch cursor from L1 Bridge adapter"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "L1 Bridge adapter snapshot unavailable at startup"
                    );
                }
            }
        }
    }

    if !seeded_from_l1 {
        warn!(
            "No executor or L1 source available; defaulting to batch_index=0. \
             The executor service must be running for TEE mode to commit/finalize batches."
        );
    }

    Ok(TeeProofManager::new(
        prover_service,
        aggregated_proof_block_jump,
        proof_sender,
        genesis_state_root.clone(),
        batch_data,
        prev_batch_hash,
        stf_info_receiver,
        shutdown_receiver,
        reqwest::Client::new(),
        oracle_url,
        midnight_bridge,
        executor_client,
        rollup_id,
        storage_path,
        rollup_url,
    )
    .post_aggregated_proof_to_da_in_background()
    .await)
}

/// Starts a process that generates aggregated proofs in the background.
pub async fn start_zk_workflow_in_background<Ps>(
    prover_service: Ps,
    aggregated_proof_block_jump: NonZero<usize>,
    proof_sender: Box<dyn ProofSender>,
    genesis_state_root: Ps::StateRoot,
    stf_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
    shutdown_receiver: tokio::sync::watch::Receiver<()>,
) -> anyhow::Result<JoinHandle<()>>
where
    Ps: ProverService,
    Ps::DaService: DaService<Error = anyhow::Error>,
{
    Ok(ZkProofManager::new(
        prover_service,
        aggregated_proof_block_jump,
        proof_sender,
        genesis_state_root,
        stf_info_receiver,
        shutdown_receiver,
    )
    .post_aggregated_proof_to_da_in_background()
    .await)
}

/// Starts the process that generates optimistic proofs in the background.
pub async fn start_op_workflow_in_background<Ps, Bps>(
    bonding_proof_service: Bps,
    proof_sender: Box<dyn ProofSender>,
    shutdown_receiver: watch::Receiver<()>,
    st_info_receiver: Receiver<Ps::StateRoot, Ps::Witness, <Ps::DaService as DaService>::Spec>,
) -> anyhow::Result<JoinHandle<()>>
where
    Ps: ProverService,
    Ps::DaService: DaService<Error = anyhow::Error>,
    Bps: BondingProofService,
{
    Ok(AttestationsManager::new(
        st_info_receiver,
        bonding_proof_service,
        proof_sender,
        shutdown_receiver,
    )
    .post_attestation_to_da_in_background()
    .await)
}

/// Starts the operator workflow in the background.
pub async fn start_operator_workflow_in_background(
    mut shutdown_receiver: watch::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _ = shutdown_receiver.changed().await;
    })
}

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
