use anyhow::Context;
use sea_orm::{ConnectOptions, Database};
use std::time::Duration;
use tracing::{info, warn};

mod api;
mod config;
mod indexer_db;
mod materialized_views;
mod metrics;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = config::Config::from_env()?;
    let da_conn = config.da_connection_string;
    let indexer_conn = config.indexer_db_connection_string;
    let ledger_api_base_url = config.ledger_api_base_url;
    let bind_addr = config.bind_addr;
    let tsink_data_path = config.tsink_data_path;
    let tsink_retention_secs = config.tsink_retention_secs;
    let tps_rounding_decimals = config.tps_rounding_decimals;
    let da_postgres_max_connections = config.da_postgres_max_connections;
    let da_postgres_min_connections = config.da_postgres_min_connections;
    let indexer_postgres_max_connections = config.indexer_postgres_max_connections;
    let indexer_postgres_min_connections = config.indexer_postgres_min_connections;
    let postgres_acquire_timeout_secs = config.postgres_acquire_timeout_secs;
    let postgres_idle_timeout_secs = config.postgres_idle_timeout_secs;
    let postgres_max_lifetime_secs = config.postgres_max_lifetime_secs;
    let transaction_size_collector_enabled = config.transaction_size_collector_enabled;
    let transaction_size_backfill_enabled = config.transaction_size_backfill_enabled;
    let materialized_view_reads_enabled = config.materialized_view_reads_enabled;
    let incremental_rollup_backfill_enabled = config.incremental_rollup_backfill_enabled;
    let materialized_view_refresh_policy = materialized_views::RefreshPolicy {
        enabled: config.materialized_view_refresh_enabled,
        refresh_on_startup: config.materialized_view_refresh_on_startup,
        interval_multiplier: config.materialized_view_refresh_interval_multiplier,
        min_interval_secs: config.materialized_view_refresh_min_interval_secs,
    };

    let ledger_http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build ledger API HTTP client")?;

    let mut connection_options = ConnectOptions::new(da_conn.clone());
    if is_postgres_connection_string(&da_conn) {
        apply_postgres_pool_options(
            &mut connection_options,
            da_postgres_max_connections,
            da_postgres_min_connections,
            postgres_acquire_timeout_secs,
            postgres_idle_timeout_secs,
            postgres_max_lifetime_secs,
        );
    }
    connection_options.sqlx_logging(false);
    let db = Database::connect(connection_options)
        .await
        .with_context(|| format!("Failed to connect DB {da_conn}"))?;

    let mut indexer_options = ConnectOptions::new(indexer_conn.clone());
    if is_postgres_connection_string(&indexer_conn) {
        apply_postgres_pool_options(
            &mut indexer_options,
            indexer_postgres_max_connections,
            indexer_postgres_min_connections,
            postgres_acquire_timeout_secs,
            postgres_idle_timeout_secs,
            postgres_max_lifetime_secs,
        );
    }
    indexer_options.sqlx_logging(false);
    let indexer_db = Database::connect(indexer_options)
        .await
        .with_context(|| format!("Failed to connect indexer DB {indexer_conn}"))?;

    metrics::rollup_state::ensure_table(&indexer_db)
        .await
        .context("Failed to initialize indexer metrics rollup state")?;
    metrics::rollup_state::ensure_table(&db)
        .await
        .context("Failed to initialize DA metrics rollup state")?;

    if materialized_view_reads_enabled || materialized_view_refresh_policy.enabled {
        tokio::spawn({
            let indexer_db = indexer_db.clone();
            let db = db.clone();

            async move {
                if let Err(error) = materialized_views::initialize_materialized_views(
                    indexer_db,
                    db,
                    materialized_view_refresh_policy,
                )
                .await
                {
                    warn!(
                        error = %error,
                        "Failed to initialize metrics materialized views"
                    );
                }
            }
        });
    } else {
        info!("Materialized view reads/refresh disabled; skipping metrics MV initialization");
    }

    let store = metrics::MetricsStore::new(tsink_data_path, tsink_retention_secs)?;
    let mut manager = metrics::MetricsManager::new(store.clone());
    // Intentionally no "average-transaction-size" collector:
    // this metric is derived at read time from MV/tsink to avoid duplicate DB polling.
    manager
        .register(
            metrics::collectors::token_value_spent::TokenValueSpentCollector::new(
                indexer_db.clone(),
                store.clone(),
                materialized_view_reads_enabled,
                incremental_rollup_backfill_enabled,
            ),
        )
        .await;
    manager
        .register(
            metrics::collectors::total_tokens_economy::TotalTokensEconomyCollector::new(
                indexer_db.clone(),
                store.clone(),
                materialized_view_reads_enabled,
                incremental_rollup_backfill_enabled,
            ),
        )
        .await;
    if transaction_size_collector_enabled {
        manager
            .register(
                metrics::collectors::transaction_size::TransactionSizeCollector::new(
                    indexer_db.clone(),
                    tsink_retention_secs,
                    transaction_size_backfill_enabled,
                ),
            )
            .await;
    } else {
        info!("Transaction size collector disabled");
    }
    manager
        .register(
            metrics::collectors::failed_transactions::FailedTransactionsCollector::new(
                db.clone(),
                store.clone(),
                materialized_view_reads_enabled,
                incremental_rollup_backfill_enabled,
            ),
        )
        .await;
    manager
        .register(
            metrics::collectors::total_transactions::TotalTransactionsCollector::new(
                db.clone(),
                store.clone(),
                materialized_view_reads_enabled,
                incremental_rollup_backfill_enabled,
            ),
        )
        .await;
    // Accounts collector for EMA metrics endpoints
    manager
        .register(metrics::collectors::accounts::AccountsCollector::new(
            indexer_db.clone(),
            db.clone(),
            store.clone(),
            materialized_view_reads_enabled,
            incremental_rollup_backfill_enabled,
        ))
        .await;
    manager.start();

    let app = api::router(api::AppState {
        store,
        retention_secs: tsink_retention_secs,
        tps_peak_cache: api::TpsPeakCache::new(),
        ema_metrics_cache: api::EmaMetricsCache::new(),
        indexer_db: indexer_db.clone(),
        materialized_view_reads_enabled,
        tps_rounding_decimals,
        ledger_api_base_url: ledger_api_base_url.clone(),
        ledger_http_client,
    });

    info!(
        "sov-metrics-api listening on {} (tps_rounding_decimals={}, ledger_api_base_url={})",
        bind_addr, tps_rounding_decimals, ledger_api_base_url
    );

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn is_postgres_connection_string(connection_string: &str) -> bool {
    connection_string.starts_with("postgres://") || connection_string.starts_with("postgresql://")
}

fn apply_postgres_pool_options(
    options: &mut ConnectOptions,
    max_connections: u32,
    min_connections: u32,
    acquire_timeout_secs: u64,
    idle_timeout_secs: u64,
    max_lifetime_secs: u64,
) {
    options
        .max_connections(max_connections)
        .min_connections(min_connections.min(max_connections))
        .acquire_timeout(Duration::from_secs(acquire_timeout_secs))
        .idle_timeout(Duration::from_secs(idle_timeout_secs))
        .max_lifetime(Duration::from_secs(max_lifetime_secs));
}
