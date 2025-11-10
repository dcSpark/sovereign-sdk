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
    run(config).await
}
