use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 1;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct AverageTransactionSizePayload {
    pub total_amount: String,
    pub total_transactions: u64,
}

pub struct AverageTransactionSizeCollector {
    db: DatabaseConnection,
}

impl AverageTransactionSizeCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl MetricCollector for AverageTransactionSizeCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "average-transaction-size",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            let (total_amount, total_transactions) =
                super::amount_aggregates::transfer_amount_totals(&self.db)
                    .await
                    .with_context(|| "Failed to aggregate average transaction size")?;

            let payload = AverageTransactionSizePayload {
                total_amount: total_amount.to_string(),
                total_transactions,
            };

            Ok(vec![MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize average transaction size payload")?,
            }])
        })
    }
}
