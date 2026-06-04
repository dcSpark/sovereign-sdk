use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait,
    FromQueryResult, PaginatorTrait, QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::materialized_views::DA_WORKER_TX_TOTALS_VIEW;
use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::{MetricSample, MetricsStore};

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct FailedTransactionsPayload {
    #[serde(alias = "total_completed")]
    pub total_transactions: u64,
    #[serde(alias = "rejected_total")]
    pub failed_transactions: u64,
}

pub struct FailedTransactionsCollector {
    db: DatabaseConnection,
    store: MetricsStore,
    materialized_view_reads_enabled: bool,
    incremental_backfill_enabled: bool,
}

impl FailedTransactionsCollector {
    pub fn new(
        db: DatabaseConnection,
        store: MetricsStore,
        materialized_view_reads_enabled: bool,
        incremental_backfill_enabled: bool,
    ) -> Self {
        Self {
            db,
            store,
            materialized_view_reads_enabled,
            incremental_backfill_enabled,
        }
    }
}

impl MetricCollector for FailedTransactionsCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "failed-transactions-rate",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            use sov_midnight_da::storable::worker_verified_transactions::{
                Column, Entity, TransactionState,
            };

            let (total_transactions, failed_transactions) = if self.materialized_view_reads_enabled
                && self.db.get_database_backend() == DatabaseBackend::Postgres
            {
                load_failed_transactions_from_mv(&self.db).await?
            } else if self.db.get_database_backend() == DatabaseBackend::Postgres {
                let totals = super::worker_tx_rollup::collect(
                    &self.db,
                    &self.store,
                    self.incremental_backfill_enabled,
                )
                .await?;
                (totals.total_transactions, totals.failed_transactions)
            } else {
                let total_transactions = Entity::find()
                    .filter(
                        Column::TransactionState
                            .is_in([TransactionState::Accepted, TransactionState::Rejected]),
                    )
                    .count(&self.db)
                    .await
                    .with_context(|| "Failed to count completed transactions")?;

                let failed_transactions = Entity::find()
                    .filter(Column::TransactionState.eq(TransactionState::Rejected))
                    .count(&self.db)
                    .await
                    .with_context(|| "Failed to count rejected transactions")?;

                (total_transactions, failed_transactions)
            };

            let payload = FailedTransactionsPayload {
                total_transactions,
                failed_transactions,
            };

            Ok(vec![MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize failed transactions payload")?,
            }])
        })
    }
}

#[derive(Debug, FromQueryResult)]
struct WorkerFailedTransactionsRow {
    total_transactions: i64,
    failed_transactions: i64,
}

async fn load_failed_transactions_from_mv(db: &DatabaseConnection) -> Result<(u64, u64)> {
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "
            SELECT total_transactions, failed_transactions
            FROM {DA_WORKER_TX_TOTALS_VIEW}
            WHERE id = 1
            "
        ),
    );

    let row = WorkerFailedTransactionsRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query worker failed tx materialized view")?
        .context("Missing worker failed tx row in materialized view")?;

    let total_transactions = u64::try_from(row.total_transactions).with_context(|| {
        format!(
            "Negative total_transactions in worker tx materialized view: {}",
            row.total_transactions
        )
    })?;
    let failed_transactions = u64::try_from(row.failed_transactions).with_context(|| {
        format!(
            "Negative failed_transactions in worker tx materialized view: {}",
            row.failed_transactions
        )
    })?;

    Ok((total_transactions, failed_transactions))
}
