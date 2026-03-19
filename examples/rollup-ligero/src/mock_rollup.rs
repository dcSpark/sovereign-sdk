use std::sync::Arc;

use async_trait::async_trait;
use demo_stf::runtime::Runtime;
use sov_address::{EthereumAddress, FromVmAddress, MultiAddressEvm};
use sov_db::ledger_db::LedgerDb;
use sov_db::storage_manager::NativeStorageManager;
use sov_ethereum::EthRpcConfig;
use sov_ligero_adapter::{Ligero, LigeroHost};
use sov_midnight_da::storable::service::StorableMidnightDaService;
use sov_midnight_da::MidnightDaSpec;
use sov_mock_zkvm::{MockCodeCommitment, MockZkvm, MockZkvmHost};
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::{Native, WitnessGeneration};
use sov_modules_api::rest::StateUpdateReceiver;
use sov_modules_api::{NodeEndpoints, Spec, Storage, SyncStatus, ZkVerifier};
use sov_modules_rollup_blueprint::pluggable_traits::PluggableSpec;
use sov_modules_rollup_blueprint::proof_sender::SovApiProofSender;
use sov_modules_rollup_blueprint::{FullNodeBlueprint, RollupBlueprint, SequencerCreationReceipt};
use sov_rollup_interface::zk::aggregated_proof::CodeCommitment;
use sov_sequencer::{ProofBlobSender, Sequencer};
use sov_stf_runner::processes::{ParallelProverService, ProverService, RollupProverConfig};
use sov_stf_runner::RollupConfig;
use tracing::warn;

use crate::eth_dev_signer;
use crate::midnight_bridge::{spawn_midnight_bridge, BridgeCursorStore};

/// Rollup with a [`ConfigurableSpec`] with [`MidnightDaSpec`] as Da spec, [`Ligero`] inner vm and [`MockZkvm`] for outer vm
#[derive(Default)]
pub struct MockDemoRollup<M> {
    phantom: std::marker::PhantomData<M>,
}

/// The default spec of the rollup
pub type MockRollupSpec<M> = ConfigurableSpec<MidnightDaSpec, Ligero, MockZkvm, MultiAddressEvm, M>;

impl RollupBlueprint<Native> for MockDemoRollup<Native>
where
    MockRollupSpec<Native>: PluggableSpec,
    <MockRollupSpec<Native> as Spec>::Address: FromVmAddress<EthereumAddress>,
{
    type Spec = MockRollupSpec<Native>;
    type Runtime = Runtime<Self::Spec>;
}

impl RollupBlueprint<WitnessGeneration> for MockDemoRollup<WitnessGeneration>
where
    MockRollupSpec<WitnessGeneration>: PluggableSpec,
    <MockRollupSpec<WitnessGeneration> as Spec>::Address: FromVmAddress<EthereumAddress>,
{
    type Spec = MockRollupSpec<WitnessGeneration>;
    type Runtime = Runtime<Self::Spec>;
}

#[async_trait]
impl FullNodeBlueprint<Native> for MockDemoRollup<Native> {
    type DaService = StorableMidnightDaService;

    type StorageManager =
        NativeStorageManager<MidnightDaSpec, <MockRollupSpec<Native> as Spec>::Storage>;

    type ProverService = ParallelProverService<
        <Self::Spec as Spec>::Address,
        <<Self::Spec as Spec>::Storage as Storage>::Root,
        <<Self::Spec as Spec>::Storage as Storage>::Witness,
        Self::DaService,
        <Self::Spec as Spec>::InnerZkvm,
        <Self::Spec as Spec>::OuterZkvm,
    >;

    type ProofSender = SovApiProofSender<Self::Spec>;

    fn create_outer_code_commitment(
        &self,
    ) -> <<Self::ProverService as ProverService>::Verifier as ZkVerifier>::CodeCommitment {
        MockCodeCommitment::default()
    }

    async fn create_endpoints(
        &self,
        state_update_receiver: StateUpdateReceiver<<Self::Spec as Spec>::Storage>,
        sync_status_receiver: tokio::sync::watch::Receiver<SyncStatus>,
        shutdown_receiver: tokio::sync::watch::Receiver<()>,
        ledger_db: &LedgerDb,
        sequencer: &SequencerCreationReceipt<Self::Spec>,
        _da_service: &Self::DaService,
        rollup_config: &RollupConfig<<Self::Spec as Spec>::Address, Self::DaService>,
    ) -> anyhow::Result<NodeEndpoints> {
        sov_modules_rollup_blueprint::register_endpoints::<Self, Native>(
            state_update_receiver.clone(),
            sync_status_receiver,
            shutdown_receiver,
            ledger_db,
            sequencer,
            rollup_config,
        )
        .await
    }

    async fn sequencer_additional_apis<Seq>(
        &self,
        sequencer: Arc<Seq>,
        rollup_config: &RollupConfig<<Self::Spec as Spec>::Address, Self::DaService>,
    ) -> anyhow::Result<NodeEndpoints>
    where
        Seq: Sequencer<Spec = Self::Spec, Rt = Self::Runtime, Da = Self::DaService>,
    {
        let eth_signer = eth_dev_signer();
        let extension = rollup_config.extension_or_panic();
        let eth_rpc_config = EthRpcConfig {
            eth_signer,
            extension: extension.clone(),
            buffer_raw_txs: true,
        };

        let mut endpoints = NodeEndpoints {
            jsonrpsee_module: sov_ethereum::get_ethereum_rpc(
                eth_rpc_config,
                Arc::clone(&sequencer),
            )
            .remove_context(),
            ..Default::default()
        };

        let cursor_store = if extension.midnight_bridge.is_some() {
            match BridgeCursorStore::open(&rollup_config.storage.path) {
                Ok(store) => Some(store),
                Err(err) => {
                    warn!(
                        error = ?err,
                        path = %rollup_config.storage.path.display(),
                        "Midnight bridge cursor persistence disabled"
                    );
                    None
                }
            }
        } else {
            None
        };

        let resolved_addr =
            sov_stf_runner::processes::bridge_lifecycle::load_contract_address(
                &rollup_config.storage.path,
            );

        let rollup_dedup_url = rollup_config
            .runner
            .http_config
            .public_address
            .clone()
            .or_else(|| {
                // Use 127.0.0.1 when bind_host is 0.0.0.0 so the bridge can reach the rollup
                // (0.0.0.0 is a server bind address, not a connectable address).
                let host = if rollup_config.runner.http_config.bind_host == "0.0.0.0" {
                    "127.0.0.1"
                } else {
                    &rollup_config.runner.http_config.bind_host
                };
                Some(format!(
                    "http://{}:{}",
                    host,
                    rollup_config.runner.http_config.bind_port
                ))
            });

        if let Some(handle) = spawn_midnight_bridge(
            Arc::clone(&sequencer),
            &extension,
            cursor_store,
            resolved_addr.as_deref(),
            rollup_dedup_url,
        )? {
            endpoints.background_handles.push(handle);
        }

        Ok(endpoints)
    }

    async fn create_da_service(
        &self,
        rollup_config: &RollupConfig<<Self::Spec as Spec>::Address, Self::DaService>,
        shutdown_receiver: tokio::sync::watch::Receiver<()>,
    ) -> Self::DaService {
        StorableMidnightDaService::from_config(rollup_config.da.clone(), shutdown_receiver).await
    }

    async fn create_prover_service(
        &self,
        prover_config: RollupProverConfig<Ligero>,
        rollup_config: &RollupConfig<<Self::Spec as Spec>::Address, Self::DaService>,
        _da_service: &Self::DaService,
    ) -> Self::ProverService {
        let (host_args, prover_config_discriminant) = prover_config.split();
        let inner_vm = LigeroHost::new(&host_args);

        let outer_vm = MockZkvmHost::new_non_blocking();
        let da_verifier = Default::default();

        ParallelProverService::new_with_default_workers(
            inner_vm,
            outer_vm,
            da_verifier,
            prover_config_discriminant,
            CodeCommitment::default(),
            rollup_config.proof_manager.prover_address,
            Some(rollup_config.storage.path.clone()),
        )
    }

    fn create_storage_manager(
        &self,
        rollup_config: &RollupConfig<<Self::Spec as Spec>::Address, Self::DaService>,
    ) -> anyhow::Result<Self::StorageManager> {
        NativeStorageManager::new(&rollup_config.storage.path)
    }

    fn create_proof_sender(
        &self,
        _rollup_config: &RollupConfig<<Self::Spec as Spec>::Address, Self::DaService>,
        sequence_number_provider: Arc<dyn ProofBlobSender>,
    ) -> anyhow::Result<Self::ProofSender> {
        Ok(Self::ProofSender::new(sequence_number_provider))
    }

    // We rely on the default create_sequencer; the worker DB is injected via env above.
}
