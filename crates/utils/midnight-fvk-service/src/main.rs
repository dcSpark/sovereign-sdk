use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ed25519_dalek::SigningKey;
use midnight_fvk_service::{create_router, generate_signing_key_hex};
use midnight_fvk_service::{log_startup, parse_hex_32, AppState, FvkIssuer, FvkStore};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Load .env from multiple locations: current dir, then crate directory
fn load_dotenv() {
    // Try current directory first
    if dotenvy::dotenv().is_ok() {
        return;
    }
    // Try the crate's directory (where Cargo.toml lives)
    let crate_dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    let env_path = crate_dir.join(".env");
    if env_path.exists() {
        let _ = dotenvy::from_path(&env_path);
    }
}

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
    #[arg(
        long,
        env = "MIDNIGHT_FVK_SERVICE_BIND",
        default_value = "127.0.0.1:8088"
    )]
    bind: SocketAddr,

    /// Database connection string for persisted state (issued FVKs + monotonic counter).
    /// Supports both SQLite and PostgreSQL:
    ///   SQLite:     sqlite://path/to/db.sqlite?mode=rwc
    ///   PostgreSQL: postgresql://user:pass@host:port/dbname
    #[arg(
        long,
        env = "MIDNIGHT_FVK_SERVICE_DB",
        default_value = "sqlite://midnight_fvk_service.sqlite?mode=rwc"
    )]
    db: String,

    /// ed25519 signing secret key (32-byte hex)
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX")]
    signing_sk_hex: Option<String>,

    /// ed25519 signing public key (32-byte hex). Must match `MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX`.
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX")]
    signing_pk_hex: Option<String>,

    /// Optional: last issued auto-index (u64). On startup we set `next_index >= last + 1`.
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_LAST_ISSUED_INDEX")]
    last_issued_index: Option<u64>,

    /// Optional: token required for private lookup endpoints (Authorization: Bearer ...).
    #[arg(long, env = "MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    let args = Args::parse();

    match args.command {
        Command::Keygen => {
            let sk_hex = generate_signing_key_hex();
            let sk_bytes = parse_hex_32("signing_sk_hex", &sk_hex)?;
            let signing_key = SigningKey::from_bytes(&sk_bytes);
            let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

            println!("MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX={}", sk_hex);
            println!("MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX={}", pk_hex);
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

    let signing_sk_hex = args.signing_sk_hex;
    let signing_pk_hex = args.signing_pk_hex;

    let signing_sk_hex = signing_sk_hex.ok_or_else(|| {
        anyhow::anyhow!(
            "missing signing key: set MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX (or run `keygen`)"
        )
    })?;
    let signing_pk_hex = signing_pk_hex.ok_or_else(|| {
        anyhow::anyhow!(
            "missing signing public key: set MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX (or run `keygen`)"
        )
    })?;

    let signing_sk_bytes = parse_hex_32("signing_sk_hex", &signing_sk_hex)?;
    let signing_pk_bytes = parse_hex_32("signing_pk_hex", &signing_pk_hex)?;

    let signing_key = SigningKey::from_bytes(&signing_sk_bytes);
    let derived_pk = *signing_key.verifying_key().as_bytes();
    anyhow::ensure!(
        derived_pk == signing_pk_bytes,
        "signing key mismatch: MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX derives pk={}, but MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX is pk={}",
        hex::encode(derived_pk),
        hex::encode(signing_pk_bytes),
    );
    let issuer = FvkIssuer::new(signing_key)?;
    let state = AppState::new(issuer, store, args.admin_token);
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
