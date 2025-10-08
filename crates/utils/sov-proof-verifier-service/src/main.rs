//! Main entry point for the proof verifier service

use anyhow::{Context, Result};
use axum::ServiceExt;
use clap::Parser;
use sov_proof_verifier_service::{create_router, AppState, ServiceConfig};
use std::net::SocketAddr;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// CLI arguments for the proof verifier service
#[derive(Debug, Parser)]
#[command(name = "proof-verifier")]
#[command(about = "Off-chain parallel proof verification service for Ligero rollup")]
struct Args {
    /// Address to bind the HTTP server to
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// URL of the rollup node RPC endpoint
    #[arg(long, default_value = "http://127.0.0.1:12346")]
    node_rpc_url: String,

    /// Path to signing key for non-ZK transactions
    #[arg(
        long,
        default_value = "../test-data/keys/token_deployer_private_key.json"
    )]
    signing_key_path: String,

    /// Ligero method ID (hex-encoded 32 bytes) for proof verification
    #[arg(long)]
    method_id: Option<String>,

    /// Chain ID for transaction authentication
    #[arg(long, default_value = "4321")]
    chain_id: u64,

    /// Maximum number of concurrent proof verifications
    #[arg(long, default_value = "10")]
    max_concurrent: usize,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing
    init_tracing(&args.log_level)?;

    info!("Starting proof verifier service");
    info!("Bind address: {}", args.bind);
    info!("Node RPC URL: {}", args.node_rpc_url);
    info!("Max concurrent verifications: {}", args.max_concurrent);

    // Parse method ID
    let method_id = if let Some(method_id_hex) = args.method_id {
        parse_method_id(&method_id_hex)?
    } else {
        // Default method ID (placeholder - should be provided)
        info!("No method ID provided, using placeholder");
        [0u8; 32]
    };

    info!("Note: Using Runtime's CHAIN_HASH for transaction signing (not CLI parameter)");

    // Create service configuration
    let config = ServiceConfig {
        node_rpc_url: args.node_rpc_url,
        signing_key_path: args.signing_key_path,
        method_id,
        chain_id: args.chain_id,
        max_concurrent_verifications: args.max_concurrent,
    };

    // Create application state (loads signing key at startup)
    let state = AppState::new(config)?;

    // Create router
    let app = create_router(state);

    // Start server
    info!("🚀 Proof verifier service listening on {}", args.bind);
    info!("📝 Endpoints:");
    info!("  POST {}/verify-and-submit", args.bind);
    info!("  GET  {}/health", args.bind);

    // Use the Axum server API
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .context("Failed to bind TCP listener")?;

    axum::serve(
        listener,
        ServiceExt::<axum::extract::Request>::into_make_service(app),
    )
    .await
    .context("Failed to serve HTTP server")?;

    Ok(())
}

/// Initialize tracing/logging
fn init_tracing(log_level: &str) -> Result<()> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    tracing_subscriber::registry()
        .with(fmt::layer().with_target(true).with_thread_ids(true))
        .with(env_filter)
        .init();

    Ok(())
}

/// Parse method ID from hex string
fn parse_method_id(hex: &str) -> Result<[u8; 32]> {
    let hex = hex.trim_start_matches("0x");
    let bytes = hex::decode(hex).context("Failed to decode method ID hex")?;

    if bytes.len() != 32 {
        anyhow::bail!("Method ID must be exactly 32 bytes, got {}", bytes.len());
    }

    let mut method_id = [0u8; 32];
    method_id.copy_from_slice(&bytes);
    Ok(method_id)
}
