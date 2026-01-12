use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use sea_orm::{ConnectOptions, Database};
use tracing::instrument::WithSubscriber;
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
mod api;
mod background_sync;
mod db;
mod index_db;
mod viewer;

use viewer::VfkRegistry;

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

    let mut connection_options = ConnectOptions::new(da_conn.clone());
    connection_options.sqlx_logging(false);
    let da_db = Database::connect(connection_options)
        .await
        .with_context(|| format!("Failed to connect DB {}", da_conn))?;
    let (idx_db, vfk_registry) = if mode == api::Mode::Sync {
        let mut connection_options = ConnectOptions::new(index_db_url.clone());
        connection_options.sqlx_logging(false);
        let idx = Database::connect(connection_options)
            .await
            .with_context(|| format!("Failed to connect index DB {}", index_db_url))?;

        println!("Initializing index database");

        db::init_index_db(&idx).await?;

        println!("index database initialized");

        // Load VFK registry for multi-address decryption (uses DashMap for lock-free access)
        let vfk_registry = load_vfk_registry(&idx).await?;
        let vfk_registry = Arc::new(vfk_registry);

        // Try a one-shot backfill; if DA tables are not ready, log and continue.
        if let Err(e) = background_sync::backfill_index(&da_db, &idx, &vfk_registry).await {
            warn!(error = %e, "Initial backfill failed; will retry in background loop");
        }
        let idx_clone = idx.clone();
        let vfk_registry_clone = vfk_registry.clone();
        tokio::spawn(async move {
            println!("Starting VFK backfill");
            if let Err(e) =
                background_sync::backfill_decrypted_recipients(&idx_clone, &vfk_registry_clone)
                    .await
            {
                warn!(error = %e, "VFK backfill failed");
            }
            println!("Finished VFK backfill");
        });
        println!("Initializing background sync loop");
        background_sync::spawn_sync_loop(da_db.clone(), idx.clone(), vfk_registry.clone());
        (idx, vfk_registry)
    } else {
        // Direct mode: create empty registry (no decryption)
        (da_db.clone(), Arc::new(VfkRegistry::new()))
    };

    if mode == api::Mode::Direct {
        info!("Indexer running in DIRECT mode; querying worker DB directly");
    } else {
        info!("Indexer running in SYNC mode; serving from index DB");
    }

    let app = api::router(api::AppState {
        db: idx_db,
        mode,
        vfk_registry,
    });

    let addr: SocketAddr = bind_addr.parse()?;
    info!("sov-indexer listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Load VFK registry from:
/// 1. VFK_CONFIG_FILE (JSON file with multiple VFKs)
/// 2. Database (previously saved VFKs)
/// 3. AUTHORITY_VFK env var (single VFK, backward compatible)
async fn load_vfk_registry(idx_db: &sea_orm::DatabaseConnection) -> anyhow::Result<VfkRegistry> {
    let mut registry = VfkRegistry::new();

    // 1. Try loading from config file
    if let Some(config_path) = viewer::load_vfk_config_path() {
        if config_path.exists() {
            match VfkRegistry::load_from_file(&config_path) {
                Ok(file_registry) => {
                    info!(
                        "Loaded {} VFKs from config file {:?}",
                        file_registry.len(),
                        config_path
                    );
                    // Save to database for persistence
                    if let Err(e) = file_registry.save_to_db(idx_db).await {
                        warn!("Failed to save VFK registry to database: {}", e);
                    }
                    registry = file_registry;
                }
                Err(e) => {
                    warn!("Failed to load VFK config file {:?}: {}", config_path, e);
                }
            }
        } else {
            warn!("VFK_CONFIG_FILE set but file not found: {:?}", config_path);
        }
    }

    // 2. Load from database (merges with any already loaded)
    match VfkRegistry::load_from_db(idx_db).await {
        Ok(db_registry) => {
            if !db_registry.is_empty() && registry.is_empty() {
                info!("Using {} VFKs from database", db_registry.len());
                registry = db_registry;
            }
        }
        Err(e) => {
            warn!("Failed to load VFK registry from database: {}", e);
        }
    }

    // 3. Fallback: single AUTHORITY_VFK env var (backward compatible)
    if registry.is_empty() {
        if let Some(vfk) = viewer::load_authority_vfk() {
            info!("Using single AUTHORITY_VFK for decryption");
            registry.add(vfk, None);
            // Save to database
            if let Err(e) = registry.save_to_db(idx_db).await {
                warn!("Failed to save single VFK to database: {}", e);
            }
        }
    }

    if registry.is_empty() {
        info!("No VFKs configured - encrypted notes will not be decrypted");
    } else {
        info!(
            "VFK registry initialized with {} keys - decryption enabled",
            registry.len()
        );
    }

    Ok(registry)
}
