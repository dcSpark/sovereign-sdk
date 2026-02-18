use anyhow::Context;
use sea_orm::{ConnectOptions, Database};
use std::time::Duration;
use tracing::info;

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
    let peak_tps_multiplier = config.peak_tps_multiplier;

    let ledger_http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build ledger API HTTP client")?;

    let mut connection_options = ConnectOptions::new(da_conn.clone());
    connection_options.sqlx_logging(false);
    let db = Database::connect(connection_options)
        .await
        .with_context(|| format!("Failed to connect DB {da_conn}"))?;

    let mut indexer_options = ConnectOptions::new(indexer_conn.clone());
    indexer_options.sqlx_logging(false);
    let indexer_db = Database::connect(indexer_options)
        .await
        .with_context(|| format!("Failed to connect indexer DB {indexer_conn}"))?;

    materialized_views::initialize_materialized_views(indexer_db.clone(), db.clone())
        .await
        .context("Failed to initialize metrics materialized views")?;

    let store = metrics::MetricsStore::new(tsink_data_path, tsink_retention_secs)?;
    let mut manager = metrics::MetricsManager::new(store.clone());
    manager
        .register(
            metrics::collectors::average_transaction_size::AverageTransactionSizeCollector::new(
                indexer_db.clone(),
            ),
        )
        .await;
    manager
        .register(
            metrics::collectors::token_value_spent::TokenValueSpentCollector::new(
                indexer_db.clone(),
            ),
        )
        .await;
    manager
        .register(
            metrics::collectors::total_tokens_economy::TotalTokensEconomyCollector::new(
                indexer_db.clone(),
            ),
        )
        .await;
    manager
        .register(
            metrics::collectors::transaction_size::TransactionSizeCollector::new(
                indexer_db.clone(),
            ),
        )
        .await;
    manager
        .register(
            metrics::collectors::failed_transactions::FailedTransactionsCollector::new(db.clone()),
        )
        .await;
    manager
        .register(
            metrics::collectors::total_transactions::TotalTransactionsCollector::new(db.clone()),
        )
        .await;
    // Accounts collector for EMA metrics endpoints
    manager
        .register(metrics::collectors::accounts::AccountsCollector::new(
            indexer_db.clone(),
            db.clone(),
        ))
        .await;
    manager.start();

    let app = api::router(api::AppState {
        store,
        retention_secs: tsink_retention_secs,
        tps_peak_cache: api::TpsPeakCache::new(),
        indexer_db: indexer_db.clone(),
        peak_tps_multiplier,
        ledger_api_base_url: ledger_api_base_url.clone(),
        ledger_http_client,
    });

    info!(
        "sov-metrics-api listening on {} (peak_tps_multiplier={}, ledger_api_base_url={})",
        bind_addr, peak_tps_multiplier, ledger_api_base_url
    );

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
