use anyhow::Context;
use sea_orm::{ConnectOptions, Database};
use tracing::info;

mod api;
mod config;
mod indexer_db;
mod metrics;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = config::Config::from_env()?;
    let da_conn = config.da_connection_string;
    let indexer_conn = config.indexer_db_connection_string;
    let bind_addr = config.bind_addr;

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

    let store = metrics::MetricsStore::new();
    let mut manager = metrics::MetricsManager::new(store.clone());
    manager
        .register(
            metrics::collectors::average_transaction_size::AverageTransactionSizeCollector::new(
                indexer_db.clone(),
            ),
        )
        .await;
    manager
        .register(metrics::collectors::token_value_spent::TokenValueSpentCollector::new(
            indexer_db.clone(),
        ))
        .await;
    manager
        .register(metrics::collectors::failed_transactions::FailedTransactionsCollector::new(
            db.clone(),
        ))
        .await;
    manager
        .register(metrics::collectors::total_transactions::TotalTransactionsCollector::new(
            db.clone(),
        ))
        .await;
    manager
        .register(metrics::collectors::tps::TpsCollector::new(db.clone()))
        .await;
    manager.start();

    let app = api::router(api::AppState { store });

    info!("sov-metrics-api listening on {}", bind_addr);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
