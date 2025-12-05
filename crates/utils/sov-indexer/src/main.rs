use anyhow::{anyhow, Context};
use sea_orm::Database;
use std::env;
use std::net::SocketAddr;
use tracing::{info, warn};
mod api;
mod background_sync;
mod db;
mod index_db;

// main only handles wiring; API, DB, sync live in modules

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    // Load .env if present (service-style env config)
    let _ = dotenvy::dotenv();
    // Read env-based config
    let da_conn = env::var("DA_CONNECTION_STRING")
        .map_err(|_| anyhow!("DA_CONNECTION_STRING env var is required"))?;
    let index_db_url = env::var("INDEX_DB")
        .unwrap_or_else(|_| "sqlite://wallet_index.sqlite?mode=rwc".to_string());
    let bind_addr = env::var("INDEXER_BIND").unwrap_or_else(|_| "0.0.0.0:13100".to_string());

    let da_db = Database::connect(&da_conn)
        .await
        .with_context(|| format!("Failed to connect DB {}", da_conn))?;

    // Index DB
    let idx_db = Database::connect(&index_db_url)
        .await
        .with_context(|| format!("Failed to connect index DB {}", index_db_url))?;
    db::init_index_db(&idx_db).await?;
    // Try a one-shot backfill; if DA tables are not ready, log and continue.
    if let Err(e) = background_sync::backfill_index(&da_db, &idx_db).await {
        warn!(error = %e, "Initial backfill failed; will retry in background loop");
    }
    background_sync::spawn_sync_loop(da_db.clone(), idx_db.clone());

    let app = api::router(api::AppState { db: idx_db.clone() });

    let addr: SocketAddr = bind_addr.parse()?;
    info!("sov-indexer listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// removed: moved to modules (db and background_sync)
