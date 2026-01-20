use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use sea_orm::{ConnectOptions, Database};
use tracing::{info, warn};
mod api;
mod background_sync;
mod balance;
mod db;
mod index_db;
mod viewer;

use viewer::FvkRegistry;

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
    let mut connection_options = ConnectOptions::new(da_conn.clone());
    connection_options.sqlx_logging(false);
    let da_db = Database::connect(connection_options)
        .await
        .with_context(|| format!("Failed to connect DB {}", da_conn))?;

    let mut connection_options = ConnectOptions::new(index_db_url.clone());
    connection_options.sqlx_logging(false);
    let idx_db = Database::connect(connection_options)
        .await
        .with_context(|| format!("Failed to connect index DB {}", index_db_url))?;

    println!("Initializing index database");

    if should_reset_index_db() {
        db::reset_index_db(&idx_db).await?;
    }

    db::init_index_db(&idx_db).await?;

    println!("index database initialized");

    // Load VFK registry for multi-address decryption (uses DashMap for lock-free access)
    let vfk_registry = load_vfk_registry(&idx_db).await?;
    let vfk_registry = Arc::new(vfk_registry);

    let fvk_service = viewer::FvkServiceClient::from_env()?;
    if vfk_registry.is_empty() {
        if fvk_service.is_some() {
            info!("FVK registry is empty; auto-fetch enabled (MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN set)");
        } else {
            info!("FVK registry is empty; encrypted notes will not be decrypted (no FVKs + no auto-fetch)");
        }
    }

    // Try a one-shot backfill; if DA tables are not ready, log and continue.
    if let Err(e) = background_sync::backfill_index(
        &da_db,
        &idx_db,
        &vfk_registry,
        fvk_service.as_ref(),
    )
    .await
    {
        warn!(error = %e, "Initial backfill failed; will retry in background loop");
    }
    let idx_clone = idx_db.clone();
    let vfk_registry_clone = vfk_registry.clone();
    let fvk_service_clone = fvk_service.clone();
    tokio::spawn(async move {
        println!("Starting encrypted-note backfills");
        if let Err(e) =
            background_sync::backfill_privacy_fields(&idx_clone, &vfk_registry_clone, fvk_service_clone.as_ref()).await
        {
            warn!(error = %e, "VFK backfill failed");
        }
        if let Err(e) = background_sync::backfill_notes_nullifiers(
            &idx_clone,
            &vfk_registry_clone,
            fvk_service_clone.as_ref(),
        )
        .await
        {
            warn!(error = %e, "notes_nullifiers backfill failed");
        }
        println!("Finished encrypted-note backfills");
    });
    println!("Initializing background sync loop");
    background_sync::spawn_sync_loop(
        da_db.clone(),
        idx_db.clone(),
        vfk_registry.clone(),
        fvk_service.clone(),
    );

    info!("Indexer running in SYNC mode; serving from index DB");

    let app = api::router(api::AppState { db: idx_db, vfk_registry });

    let addr: SocketAddr = bind_addr.parse()?;
    info!("sov-indexer listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn should_reset_index_db() -> bool {
    matches!(
        env::var("INDEX_DB_RESET").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

/// Load VFK registry from:
/// 1. VFK_CONFIG_FILE (JSON file with multiple VFKs)
/// 2. Database (previously saved VFKs)
async fn load_vfk_registry(idx_db: &sea_orm::DatabaseConnection) -> anyhow::Result<FvkRegistry> {
    let mut registry = FvkRegistry::new();

    // 1. Try loading from config file
    if let Some(config_path) = viewer::load_fvk_config_path() {
        if config_path.exists() {
            match FvkRegistry::load_from_file(&config_path) {
                Ok(file_registry) => {
                    info!(
                        "Loaded {} FVKs from config file {:?}",
                        file_registry.len(),
                        config_path
                    );
                    // Save to database for persistence
                    if let Err(e) = file_registry.save_to_db(idx_db).await {
                        warn!("Failed to save FVK registry to database: {}", e);
                    }
                    registry = file_registry;
                }
                Err(e) => {
                    warn!("Failed to load FVK config file {:?}: {}", config_path, e);
                }
            }
        } else {
            warn!("FVK_CONFIG_FILE set but file not found: {:?}", config_path);
        }
    }

    // 2. Load from database (merges with any already loaded)
    match FvkRegistry::load_from_db(idx_db).await {
        Ok(db_registry) => {
            if !db_registry.is_empty() && registry.is_empty() {
                info!("Using {} FVKs from database", db_registry.len());
                registry = db_registry;
            }
        }
        Err(e) => {
            warn!("Failed to load FVK registry from database: {}", e);
        }
    }

    if registry.is_empty() {
        info!("No FVKs preconfigured (fvk_registry is empty)");
    } else {
        info!(
            "FVK registry initialized with {} keys - decryption enabled",
            registry.len()
        );
    }

    Ok(registry)
}
