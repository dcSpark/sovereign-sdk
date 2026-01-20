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
            name: "total_transactions",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
            max_samples: MAX_SAMPLES,
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<MetricSample>> {
        Box::pin(async move {
            use sov_midnight_da::storable::worker_verified_transactions::{
                Column, Entity, TransactionState,
            };

            let total_transactions = Entity::find().count(&self.db).await?;

            let payload = TotalTransactionsPayload { total_transactions };

            Ok(MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize total transactions payload")?,
            })
        })
    }
}
