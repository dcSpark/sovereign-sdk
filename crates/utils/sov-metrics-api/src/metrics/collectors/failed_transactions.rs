use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
pub const RETENTION_SECONDS: u64 = 300;
pub const MAX_SAMPLES: usize = (RETENTION_SECONDS / SAMPLE_INTERVAL_SECS) as usize;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct FailedTransactionsPayload {
    #[serde(alias = "total_completed")]
    pub total_transactions: u64,
    #[serde(alias = "rejected_total")]
    pub failed_transactions: u64,
}

pub struct FailedTransactionsCollector {
    db: DatabaseConnection,
}

impl FailedTransactionsCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl MetricCollector for FailedTransactionsCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "failed-transactions-rate",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
            max_samples: MAX_SAMPLES,
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            use sov_midnight_da::storable::worker_verified_transactions::{
                Column, Entity, TransactionState,
            };

            let total_paginator = Entity::find()
                .filter(Column::TransactionState.is_in([
                    TransactionState::Accepted,
                    TransactionState::Rejected,
                ]))
                .paginate(&self.db, 1);
            let total_transactions = total_paginator
                .num_items()
                .await
                .with_context(|| "Failed to count completed transactions")?;

            let rejected_paginator = Entity::find()
                .filter(Column::TransactionState.eq(TransactionState::Rejected))
                .paginate(&self.db, 1);
            let failed_transactions = rejected_paginator
                .num_items()
                .await
                .with_context(|| "Failed to count rejected transactions")?;

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
