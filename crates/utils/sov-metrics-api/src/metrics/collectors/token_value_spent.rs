use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::indexer_db::midnight_transfer;
use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
pub const RETENTION_SECONDS: u64 = 300;
pub const MAX_SAMPLES: usize = (RETENTION_SECONDS / SAMPLE_INTERVAL_SECS) as usize;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TokenValueSpentPayload {
    pub total_amount: String,
    pub total_transactions: u64,
}

pub struct TokenValueSpentCollector {
    db: DatabaseConnection,
}

impl TokenValueSpentCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl MetricCollector for TokenValueSpentCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "token-value-spent",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
            max_samples: MAX_SAMPLES,
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<MetricSample>> {
        Box::pin(async move {
            let rows = midnight_transfer::Entity::find()
                .filter(midnight_transfer::Column::Amount.is_not_null())
                .all(&self.db)
                .await
                .with_context(|| "Failed to load midnight_transfer rows")?;

            let mut total_amount: u128 = 0;
            let mut total_transactions: u64 = 0;

            for row in rows {
                let amount = row
                    .amount
                    .as_ref()
                    .with_context(|| "Missing transfer amount")?
                    .parse::<u128>()
                    .with_context(|| "Invalid transfer amount")?;
                total_amount = total_amount
                    .checked_add(amount)
                    .with_context(|| "Transfer amount overflow")?;
                total_transactions += 1;
            }

            let payload = TokenValueSpentPayload {
                total_amount: total_amount.to_string(),
                total_transactions,
            };

            Ok(MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize token value spent payload")?,
            })
        })
    }
}
