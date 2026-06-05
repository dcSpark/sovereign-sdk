//! Computes and prints rollup genesis info without starting the full node.
//!
//! Use this to get Bridge `initialize` values before rollup startup:
//! - `genesisStateRoot_` (first 32 bytes of rollup genesis state root)
//! - `genesisBatchHash_` (current rollup/bridge convention: 32-byte zero hash)
//!
//! Example:
//!   cargo run --bin print-genesis-info -- --rollup-config rollup_config_tee_local.toml --genesis-dir demo_data_tee/genesis

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use demo_stf::genesis_config::GenesisPaths;
use sov_address::MultiAddressEvm;
use sov_db::storage_manager::NativeStorageManager;
use sov_midnight_da::storable::service::StorableMidnightDaService;
use sov_midnight_da::{BlockProducingConfig, MidnightDaConfig};
use sov_modules_api::execution_mode::Native;
use sov_modules_rollup_blueprint::FullNodeBlueprint;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_modules_stf_blueprint::StfBlueprint;
use sov_rollup_interface::node::da::DaService;
use sov_rollup_ligero::MockDemoRollup;
use sov_stf_runner::{from_toml_path, initialize_state, RollupConfig};
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(about = "Print rollup genesis info for L1 Bridge initialization")]
struct Args {
    /// Path to the rollup config TOML (used for genesis params and genesis height).
    #[arg(long, default_value = "rollup_config.toml")]
    rollup_config: String,

    /// Path to the genesis config directory (JSON files).
    #[arg(long)]
    genesis_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let rollup_config: RollupConfig<MultiAddressEvm, StorableMidnightDaService> =
        from_toml_path(&args.rollup_config)
            .with_context(|| format!("Failed to read rollup config from {}", args.rollup_config))?;

    let genesis_paths = GenesisPaths::from_dir(&args.genesis_dir);
    let blueprint = MockDemoRollup::<Native>::default();

    let genesis_params = blueprint
        .create_genesis_config(&genesis_paths, &rollup_config)
        .context("Failed to create genesis config from genesis dir")?;

    let genesis_height = rollup_config.runner.genesis_height;

    let temp_state_dir = tempfile::tempdir().context("Failed to create temp dir for state")?;
    let mut storage_config = rollup_config.storage.clone();
    storage_config.path = temp_state_dir.path().to_path_buf();

    let mut temp_rollup_config = rollup_config.clone();
    temp_rollup_config.storage = storage_config;
    let mut da_config = rollup_config.da.clone();
    da_config.connection_string = MidnightDaConfig::sqlite_in_memory();
    da_config.block_producing = BlockProducingConfig::Manual;
    da_config.da_layer = None;
    temp_rollup_config.da = da_config;

    let mut storage_manager = blueprint
        .create_storage_manager(&temp_rollup_config)
        .context("Failed to create storage manager")?;

    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let da_service = StorableMidnightDaService::from_config(temp_rollup_config.da.clone(), shutdown_rx).await;

    let genesis_block = da_service
        .get_block_at(genesis_height)
        .await
        .with_context(|| format!("Failed to get genesis block at height {}", genesis_height))?;

    type Spec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;
    type Runtime = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Runtime;
    let native_stf = StfBlueprint::<Spec, Runtime>::new();

    let genesis_state_root = initialize_state::<
        StfBlueprint<Spec, Runtime>,
        <Spec as sov_modules_api::Spec>::InnerZkvm,
        <Spec as sov_modules_api::Spec>::OuterZkvm,
        StorableMidnightDaService,
        NativeStorageManager<sov_midnight_da::MidnightDaSpec, <Spec as sov_modules_api::Spec>::Storage>,
    >(&native_stf, &mut storage_manager, genesis_block, genesis_params)
    .await
    .context("Failed to initialize state (compute genesis root)")?;

    let root_bytes = genesis_state_root.as_ref();
    let genesis_state_root_32: [u8; 32] = root_bytes[..32]
        .try_into()
        .expect("genesis root is at least 32 bytes");

    // Current bridge/rollup convention for genesis batch hash.
    let genesis_batch_hash = [0u8; 32];

    println!("Bridge initialize values:");
    println!("  genesisStateRoot_={}", hex::encode(genesis_state_root_32));
    println!("  genesisBatchHash_={}", hex::encode(genesis_batch_hash));
    println!();
    println!("Diagnostics:");
    println!("  fullGenesisStateRoot64={}", hex::encode(root_bytes));

    Ok(())
}
