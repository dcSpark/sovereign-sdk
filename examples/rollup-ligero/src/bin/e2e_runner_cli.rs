use anyhow::Result;
use clap::Parser;
use sov_rollup_ligero::e2e_runner::{run, RunnerConfig};

/// CLI wrapper around the E2E benchmark runner.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Sequencer REST URL to reuse instead of spawning a local node.
    #[arg(long)]
    node_url: Option<String>,
    /// Proof verifier URL to reuse instead of spawning a local service.
    #[arg(long)]
    verifier_url: Option<String>,
    /// Number of deposits/transfers to submit.
    #[arg(long)]
    num_deposits: Option<usize>,
    /// Enable proof caching (disabled by default)
    #[arg(long)]
    cache: bool,
    /// Directory to store cached proofs (default: proof_cache)
    #[arg(long)]
    cache_dir: Option<String>,
    /// Enable local proof verification (disabled by default)
    #[arg(long)]
    verify: bool,
    /// Queue verifier submissions and flush to sequencer in batches (test mode)
    #[arg(long)]
    batch_submit: bool,
    /// Delay (ms) between transfer submissions to the verifier
    #[arg(long)]
    transfer_delay_ms: Option<u64>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = RunnerConfig::from_env();
    if let Some(n) = cli.num_deposits {
        config.num_deposits = n;
    }
    if let Some(node) = cli.node_url {
        config.external_node_url = Some(node);
    }
    if let Some(verifier) = cli.verifier_url {
        config.external_verifier_url = Some(verifier);
    }
    if cli.cache {
        config.use_proof_cache = true;
    }
    if let Some(dir) = cli.cache_dir {
        config.proof_cache_dir = dir.into();
    }
    if cli.verify {
        config.skip_verify = false;
    }
    if cli.batch_submit {
        config.defer_sequencer_submission = true;
    }
    if let Some(ms) = cli.transfer_delay_ms {
        config.transfer_submit_delay_ms = ms;
    }
    run(config).await
}
