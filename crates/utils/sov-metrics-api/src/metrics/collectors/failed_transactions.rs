use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
pub const WINDOW_SECONDS: u64 = SAMPLE_INTERVAL_SECS;
pub const RETENTION_SECONDS: u64 = 300;
pub const MAX_SAMPLES: usize = (RETENTION_SECONDS / SAMPLE_INTERVAL_SECS) as usize;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct FailedTransactionsPayload {
    pub total_completed: u64,
    pub rejected: u64,
    pub rejected_rate_percent: f64,
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

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<MetricSample>> {
        Box::pin(async move {
            use sov_midnight_da::storable::worker_verified_transactions::{
                Column, Entity, TransactionState,
            };

            let window_end = Utc::now();
            let window_start = window_end - ChronoDuration::seconds(WINDOW_SECONDS as i64);

            let total_paginator = Entity::find()
                .filter(Column::TransactionState.is_in([
                    TransactionState::Accepted,
                    TransactionState::Rejected,
                ]))
                .filter(Column::CreatedAt.gte(window_start))
                .filter(Column::CreatedAt.lt(window_end))
                .paginate(&self.db, 1);
            let total_completed = total_paginator
                .num_items()
                .await
                .with_context(|| "Failed to count completed transactions")?;

            let rejected_paginator = Entity::find()
                .filter(Column::TransactionState.eq(TransactionState::Rejected))
                .filter(Column::CreatedAt.gte(window_start))
                .filter(Column::CreatedAt.lt(window_end))
                .paginate(&self.db, 1);
            let rejected = rejected_paginator
                .num_items()
                .await
                .with_context(|| "Failed to count rejected transactions")?;

            let rejected_rate_percent = if total_completed == 0 {
                0.0
            } else {
                (rejected as f64 / total_completed as f64) * 100.0
            };

            let payload = FailedTransactionsPayload {
                total_completed,
                rejected,
                rejected_rate_percent,
            };

            Ok(MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize failed transactions payload")?,
            })
        })
    }
}
