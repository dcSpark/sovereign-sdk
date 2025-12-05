use anyhow::Result;
use clap::Parser;
use midnight_e2e_benchmarks::e2e_runner::{run, RunnerConfig};

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
    /// Authority Full Viewing Key (32-byte hex) for Level-B compliance.
    /// When set, transfer proofs include viewer attestations and txs include encrypted notes.
    /// Can also be set via AUTHORITY_FVK environment variable.
    #[arg(long, env = "AUTHORITY_FVK")]
    authority_fvk: Option<String>,
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
    // Parse authority FVK from CLI arg (overrides env var if provided)
    if let Some(fvk_hex) = cli.authority_fvk {
        let s = fvk_hex.trim();
        let s = s.strip_prefix("0x").unwrap_or(s);
        match hex::decode(s) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut fvk = [0u8; 32];
                fvk.copy_from_slice(&bytes);
                config.authority_fvk = Some(fvk);
            }
            Ok(_) => {
                eprintln!("[warn] --authority-fvk must be 32 bytes (64 hex chars), ignoring");
            }
            Err(e) => {
                eprintln!("[warn] --authority-fvk invalid hex: {e}, ignoring");
            }
        }
    }
    run(config).await
}
