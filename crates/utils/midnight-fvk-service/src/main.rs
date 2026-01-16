use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use k256::ecdsa::SigningKey;
use midnight_fvk_service::{create_router, generate_root_seed_hex, generate_signing_key_hex};
use midnight_fvk_service::{log_startup, parse_hex_32, AppState, FvkIssuer, FvkStore};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug, Parser)]
#[command(name = "midnight-fvk-service")]
#[command(about = "Issues Midnight FVKs with signed Poseidon2 commitments")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate fresh secrets and print `.env` lines
    Keygen,
    /// Run the HTTP server
    Serve(ServeArgs),
}

#[derive(Debug, Parser)]
struct ServeArgs {
    /// Address to bind the HTTP server to
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_BIND", default_value = "127.0.0.1:8088")]
    bind: SocketAddr,

    /// SQLite connection string for persisted state (issued FVKs + monotonic counter)
    #[arg(
        long,
        env = "MIDNIGHT_FVK_SERVICE_DB",
        default_value = "sqlite://midnight_fvk_service.sqlite?mode=rwc"
    )]
    db: String,

    /// secp256k1 signing secret key (32-byte hex)
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX")]
    signing_sk_hex: Option<String>,

    /// Root seed used to deterministically derive FVKs (32-byte hex)
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_ROOT_FVK_SEED_HEX")]
    root_fvk_seed_hex: Option<String>,

    /// Optional: last issued auto-index (u64). On startup we set `next_index >= last + 1`.
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_LAST_ISSUED_INDEX")]
    last_issued_index: Option<u64>,

    /// When true, missing keys are generated in-memory at startup
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_ALLOW_EPHEMERAL_KEYS", default_value_t = false)]
    allow_ephemeral_keys: bool,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = Args::parse();

    match args.command {
        Command::Keygen => {
            println!(
                "MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX={}",
                generate_signing_key_hex()
            );
            println!(
                "MIDNIGHT_FVK_SERVICE_ROOT_FVK_SEED_HEX={}",
                generate_root_seed_hex()
            );
            Ok(())
        }
        Command::Serve(serve_args) => serve(serve_args).await,
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    init_tracing(&args.log_level)?;

    let store = FvkStore::new(&args.db).await?;
    if let Some(last) = args.last_issued_index {
        let min_next = last.saturating_add(1);
        let _ = store.ensure_next_index_at_least(min_next).await?;
    }

    let signing_sk_hex = args.signing_sk_hex.or_else(|| {
        args.allow_ephemeral_keys
            .then(|| generate_signing_key_hex())
    });
    let root_fvk_seed_hex = args.root_fvk_seed_hex.or_else(|| {
        args.allow_ephemeral_keys
            .then(|| generate_root_seed_hex())
    });

    let signing_sk_hex = signing_sk_hex.ok_or_else(|| {
        anyhow::anyhow!(
            "missing signing key: set MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX (or run `keygen`)"
        )
    })?;
    let root_fvk_seed_hex = root_fvk_seed_hex.ok_or_else(|| {
        anyhow::anyhow!(
            "missing root seed: set MIDNIGHT_FVK_SERVICE_ROOT_FVK_SEED_HEX (or run `keygen`)"
        )
    })?;

    let signing_sk_bytes = parse_hex_32("signing_sk_hex", &signing_sk_hex)?;
    let root_fvk_seed = parse_hex_32("root_fvk_seed_hex", &root_fvk_seed_hex)?;

    let signing_key = SigningKey::from_bytes(&signing_sk_bytes.into())
        .map_err(|e| anyhow::anyhow!("invalid signing key: {e}"))?;
    let issuer = FvkIssuer::new(signing_key, root_fvk_seed)?;
    let state = AppState::new(issuer, store);
    let app = create_router(state);

    log_startup(args.bind);

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .context("Failed to bind TCP listener")?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Failed to serve HTTP server")?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_tracing(level: &str) -> Result<()> {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .try_init()
        .context("Failed to initialize tracing")?;
    Ok(())
}
