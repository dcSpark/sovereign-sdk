use anyhow::{anyhow, Context};
use sea_orm::Database;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, warn};
mod api;
mod background_sync;
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
    let mode = match env::var("MODE")
        .unwrap_or_else(|_| "direct".to_string())
        .to_lowercase()
        .as_str()
    {
        "sync" => api::Mode::Sync,
        _ => api::Mode::Direct,
    };

    let da_db = Database::connect(&da_conn)
        .await
        .with_context(|| format!("Failed to connect DB {}", da_conn))?;

    let (idx_db, fvk_registry) = if mode == api::Mode::Sync {
        let idx = Database::connect(&index_db_url)
            .await
            .with_context(|| format!("Failed to connect index DB {}", index_db_url))?;
        db::init_index_db(&idx).await?;

        // Load FVK registry for multi-address decryption (uses DashMap for lock-free access)
        let fvk_registry = load_fvk_registry(&idx).await?;
        let fvk_registry = Arc::new(fvk_registry);

        // Try a one-shot backfill; if DA tables are not ready, log and continue.
        if let Err(e) = background_sync::backfill_index(&da_db, &idx, &fvk_registry).await {
            warn!(error = %e, "Initial backfill failed; will retry in background loop");
        }
        background_sync::spawn_sync_loop(da_db.clone(), idx.clone(), fvk_registry.clone());
        (idx, fvk_registry)
    } else {
        // Direct mode: create empty registry (no decryption)
        (da_db.clone(), Arc::new(FvkRegistry::new()))
    };

    if mode == api::Mode::Direct {
        info!("Indexer running in DIRECT mode; querying worker DB directly");
    } else {
        info!("Indexer running in SYNC mode; serving from index DB");
    }

    let app = api::router(api::AppState {
        db: idx_db,
        mode,
        fvk_registry,
    });

    let addr: SocketAddr = bind_addr.parse()?;
    info!("sov-indexer listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Load FVK registry from:
/// 1. FVK_CONFIG_FILE (JSON file with multiple FVKs)
/// 2. Database (previously saved FVKs)
/// 3. AUTHORITY_FVK env var (single FVK, backward compatible)
async fn load_fvk_registry(idx_db: &sea_orm::DatabaseConnection) -> anyhow::Result<FvkRegistry> {
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

    // 3. Fallback: single AUTHORITY_FVK env var (backward compatible)
    if registry.is_empty() {
        if let Some(fvk) = viewer::load_authority_fvk() {
            info!("Using single AUTHORITY_FVK for decryption");
            registry.add(fvk, None);
            // Save to database
            if let Err(e) = registry.save_to_db(idx_db).await {
                warn!("Failed to save single FVK to database: {}", e);
            }
        }
    }

    if registry.is_empty() {
        info!("No FVKs configured - encrypted notes will not be decrypted");
    } else {
        info!(
            "FVK registry initialized with {} keys - decryption enabled",
            registry.len()
        );
    }

    Ok(registry)
}
