//! Main entry point for the proof verifier service

use anyhow::{Context, Result};
use axum::ServiceExt;
use clap::Parser;
use sov_address::MultiAddressEvm;
use sov_midnight_da::storable::service::StorableMidnightDaService;
use sov_midnight_da::storable::IncomingWorkerTxSaver;
use sov_proof_verifier_service::{create_router, AppState, ServiceConfig};
use sov_stf_runner::{from_toml_path, RollupConfig};
use std::net::SocketAddr;
use std::path::Path;
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

    /// Path to the rollup configuration TOML used by the rollup node.
    /// When set, the worker transactions DB connection string will be read from this file's
    /// [da] section `connection_string` and the incoming worker-tx persistence settings
    /// (`save_incoming_worker_txs`, `worker_tx_path`, `worker_tx_bucket`) will be applied.
    #[arg(long = "rollup-config-path")]
    rollup_config_path: Option<String>,

    /// Path to signing key for non-ZK transactions
    #[arg(
        long,
        default_value = "../test-data/keys/token_deployer_private_key.json"
    )]
    signing_key_path: String,

    /// Ligero method ID (hex-encoded 32 bytes) for value-setter proof verification
    #[arg(long)]
    method_id: Option<String>,

    /// Ligero method ID (hex-encoded 32 bytes) for midnight proof verification
    #[arg(long)]
    midnight_method_id: Option<String>,

    /// Chain ID for transaction authentication
    #[arg(long, default_value = "4321")]
    chain_id: u64,

    /// Maximum number of concurrent proof verifications
    #[arg(long, default_value = "5")]
    max_concurrent: usize,

    /// Connection string for the worker_txs database (used to store worker_verified_transactions).
    /// If not provided, the service will try to read it from --rollup-config-path's [da] section.
    /// If neither is set, it falls back to the demo default
    /// "sqlite://examples/rollup-ligero/demo_data/worker_txs.sqlite?mode=rwc".
    #[arg(long)]
    da_db: Option<String>,

    /// When set, the service will queue worker-verified txs instead of immediately submitting
    /// them to the sequencer. Use the /midnight-privacy/flush endpoint to release queued txs.
    #[arg(long, default_value_t = false)]
    defer_submission: bool,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing
    init_tracing(&args.log_level)?;

    // Load rollup config once (if provided) so we can derive both the DA DB connection
    // string and the incoming-worker-tx persistence settings.
    let rollup_config: Option<RollupConfig<MultiAddressEvm, StorableMidnightDaService>> =
        if let Some(ref config_path) = args.rollup_config_path {
            info!("Loading rollup config from {}", config_path);
            Some(from_toml_path(config_path).with_context(|| {
                format!("Failed to read rollup configuration from {}", config_path)
            })?)
        } else {
            None
        };

    // Resolve worker_txs DB connection string, preferring explicit CLI value, then rollup_config.toml, then demo default.
    let da_connection_string = match args.da_db.clone() {
        Some(explicit) => explicit,
        None => {
            if let (Some(ref config_path), Some(ref rollup_config)) =
                (args.rollup_config_path.as_ref(), rollup_config.as_ref())
            {
                let conn = normalize_sqlite_connection_path(
                    config_path,
                    rollup_config.da.connection_string.clone(),
                )?;

                info!(
                    "Using worker_txs DB connection string from rollup config: {}",
                    conn
                );

                conn
            } else {
                let default_conn =
                    "sqlite://examples/rollup-ligero/demo_data/worker_txs.sqlite?mode=rwc"
                        .to_string();
                info!(
                    "No --da-db or --rollup-config-path provided; falling back to default worker_txs DB: {}",
                    default_conn
                );
                default_conn
            }
        }
    };

    let incoming_worker_tx_saver = if let (Some(ref config_path), Some(ref rollup_config)) =
        (args.rollup_config_path.as_ref(), rollup_config.as_ref())
    {
        let config_dir = Path::new(config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        IncomingWorkerTxSaver::from_config(&rollup_config.da, Some(config_dir)).await?
    } else {
        IncomingWorkerTxSaver::disabled()
    };

    info!("Starting proof verifier service");
    info!("Bind address: {}", args.bind);
    info!("Node RPC URL: {}", args.node_rpc_url);
    info!("Max concurrent verifications: {}", args.max_concurrent);
    info!("Worker transactions DB: {}", da_connection_string);
    info!("Defer submission: {}", args.defer_submission);

    // Parse optional method ID (will be auto-computed if not provided)
    let value_setter_method_id = if let Some(method_id_hex) = args.method_id {
        Some(parse_method_id(&method_id_hex)?)
    } else {
        info!(
            "No value-setter method ID provided, will auto-compute from value_validator_rust.wasm"
        );
        None
    };

    // Parse optional midnight method ID (will be auto-computed if not provided)
    let midnight_method_id = if let Some(method_id_hex) = args.midnight_method_id {
        Some(parse_method_id(&method_id_hex)?)
    } else {
        info!("No midnight method ID provided, will auto-compute from note_spend_guest.wasm");
        None
    };

    info!("Note: Using /rollup/schema chain_hash for transaction signing/verification");

    // Create service configuration
    let config = ServiceConfig {
        node_rpc_url: args.node_rpc_url,
        signing_key_path: args.signing_key_path,
        value_setter_method_id, // Will be auto-computed from value_validator_rust.wasm if None
        midnight_method_id,     // Will be auto-computed from note_spend_guest.wasm if None
        chain_id: args.chain_id,
        max_concurrent_verifications: args.max_concurrent,
        da_connection_string,
        defer_sequencer_submission: args.defer_submission,
    };

    // Create application state (loads signing key at startup)
    let state =
        AppState::new_with_incoming_worker_tx_saver(config, incoming_worker_tx_saver).await?;

    // Create router
    let app = create_router(state);

    // Start server
    info!("🚀 Proof verifier service listening on {}", args.bind);
    info!("📝 Endpoints:");
    info!("  POST {}/value-setter-zk", args.bind);
    info!("  POST {}/midnight-privacy", args.bind);
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

/// Convert relative SQLite paths (e.g. `sqlite://demo_data/worker_txs.sqlite`) into absolute
/// paths rooted at the directory containing `rollup_config_path`. Non-SQLite strings are
/// returned unchanged.
fn normalize_sqlite_connection_path(
    rollup_config_path: &str,
    connection_string: String,
) -> Result<String> {
    if let Some(stripped) = connection_string.strip_prefix("sqlite://") {
        // Leave special paths (absolute, :memory:, etc.) alone.
        if stripped.starts_with('/') {
            return Ok(connection_string);
        }

        let (path_part, query) = match stripped.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (stripped, None),
        };

        let path = Path::new(path_part);
        if path.is_absolute() {
            return Ok(connection_string);
        }

        // Resolve relative path against the directory containing the rollup config file.
        let config_dir = Path::new(rollup_config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let resolved_path = config_dir.join(path);
        let resolved_str = resolved_path.to_string_lossy();

        let mut rebuilt = format!("sqlite://{}", resolved_str);
        if let Some(q) = query {
            if !q.is_empty() {
                rebuilt.push('?');
                rebuilt.push_str(q);
            }
        }
        Ok(rebuilt)
    } else {
        Ok(connection_string)
    }
}
