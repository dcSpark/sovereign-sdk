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
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TotalTransactionsPayload {
    pub total_transactions: u64,
}

pub struct TotalTransactionsCollector {
    db: DatabaseConnection,
}

impl TotalTransactionsCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl MetricCollector for TotalTransactionsCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "total-transactions",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            use sov_midnight_da::storable::worker_verified_transactions::{
                Column, Entity, TransactionState,
            };

            let total_transactions = if self.db.get_database_backend() == DatabaseBackend::Postgres
            {
                load_total_transactions_from_mv(&self.db).await?
            } else {
                Entity::find()
                    .filter(
                        Column::TransactionState
                            .is_in([TransactionState::Accepted, TransactionState::Rejected]),
                    )
                    .count(&self.db)
                    .await
                    .with_context(|| "Failed to count completed transactions")?
            };

            let payload = TotalTransactionsPayload { total_transactions };

            Ok(vec![MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize total transactions payload")?,
            }])
        })
    }
}

#[derive(Debug, FromQueryResult)]
struct WorkerTotalTransactionsRow {
    total_transactions: i64,
}

async fn load_total_transactions_from_mv(db: &DatabaseConnection) -> Result<u64> {
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "
            SELECT total_transactions
            FROM {DA_WORKER_TX_TOTALS_VIEW}
            WHERE id = 1
            "
        ),
    );

    let row = WorkerTotalTransactionsRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query worker tx totals materialized view")?
        .context("Missing worker tx totals row in materialized view")?;

    u64::try_from(row.total_transactions).with_context(|| {
        format!(
            "Negative total_transactions in worker tx totals materialized view: {}",
            row.total_transactions
        )
    })
}
