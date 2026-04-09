use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use demo_stf::genesis_config::GenesisPaths;
use sov_address::MultiAddressEvm;
use sov_midnight_da::storable::service::StorableMidnightDaService;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::RollupHeight;
use sov_modules_api::execution_mode::Native;
use sov_modules_rollup_blueprint::logging::initialize_logging;
use sov_modules_rollup_blueprint::{FullNodeBlueprint, Rollup};
use sov_rollup_nightstream::NightstreamRollup;
use sov_stf_runner::processes::{RollupProverConfig, RollupProverConfigDiscriminants};
use sov_stf_runner::{from_toml_path, RollupConfig};
use tracing::debug;

/// Nightstream rollup runner using Midnight DA
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The path to the rollup config.
    #[arg(long, default_value = "rollup_config.toml")]
    rollup_config_path: String,

    /// The path to the genesis configs.
    #[arg(long, default_value = "../test-data/genesis/demo/mock")]
    genesis_config_dir: PathBuf,

    /// Listen address for Prometheus exporter.
    #[arg(long, default_value = "127.0.0.1:9845")]
    prometheus_exporter_bind: String,

    /// Stops the rollup at a given height.
    #[arg(long, default_value = None)]
    stop_at_rollup_height: Option<u64>,

    /// Asserts that the rollup starts at a given height.
    #[arg(long, default_value = None)]
    start_at_rollup_height: Option<u64>,
}

#[tokio::main]
async fn main() {
    let _guard = initialize_logging();

    match run().await {
        Ok(_) => {
            tracing::debug!("Rollup execution complete. Shutting down.");
        }
        Err(e) => {
            tracing::error!(error = ?e, backtrace= e.backtrace().to_string(), "Rollup execution failed");
            exit(1);
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    prometheus_exporter::start(args.prometheus_exporter_bind.parse()?)
        .context("Prometheus exporter start failed")?;

    let rollup_config_path = args.rollup_config_path.as_str();

    let prover_config_disc = parse_prover_config().expect("Failed to parse prover config");
    tracing::info!(
        ?prover_config_disc,
        "Running Nightstream rollup with prover config"
    );

    let start_at_rollup_height = args.start_at_rollup_height.map(RollupHeight::new);
    let stop_at_rollup_height = args.stop_at_rollup_height.map(RollupHeight::new);

    let prover_config = prover_config_disc.map(|config_disc| config_disc.into_config(Arc::new(())));

    let rollup = new_rollup(
        &GenesisPaths::from_dir(&args.genesis_config_dir),
        rollup_config_path,
        prover_config,
        start_at_rollup_height,
        stop_at_rollup_height,
    )
    .await
    .context("Failed to initialize Nightstream rollup")?;

    rollup.run().await
}

fn parse_prover_config() -> anyhow::Result<Option<RollupProverConfigDiscriminants>> {
    if let Some(value) = option_env!("SOV_PROVER_MODE") {
        let config = std::str::FromStr::from_str(value).inspect_err(|&error| {
            tracing::error!(value, ?error, "Unknown `SOV_PROVER_MODE` value; aborting");
        })?;
        #[cfg(debug_assertions)]
        {
            if config == RollupProverConfigDiscriminants::Prove {
                tracing::warn!(prover_config = ?config, "Given RollupProverConfig might cause slow rollup progression if not compiled in release mode.");
            }
        }
        Ok(Some(config))
    } else {
        Ok(None)
    }
}

async fn new_rollup(
    rt_genesis_paths: &GenesisPaths,
    rollup_config_path: &str,
    prover_config: Option<RollupProverConfig<MockZkvm>>,
    start_at_rollup_height: Option<RollupHeight>,
    stop_at_rollup_height: Option<RollupHeight>,
) -> anyhow::Result<Rollup<NightstreamRollup<Native>, Native>> {
    debug!(
        config_path = rollup_config_path,
        "Starting Nightstream rollup on Midnight DA"
    );

    let rollup_config: RollupConfig<MultiAddressEvm, StorableMidnightDaService> =
        from_toml_path(rollup_config_path).with_context(|| {
            format!("Failed to read rollup configuration from {rollup_config_path}")
        })?;

    let rollup = NightstreamRollup::<Native>::default();
    rollup
        .create_new_rollup(
            rt_genesis_paths,
            rollup_config,
            prover_config,
            start_at_rollup_height,
            stop_at_rollup_height,
        )
        .await
}
