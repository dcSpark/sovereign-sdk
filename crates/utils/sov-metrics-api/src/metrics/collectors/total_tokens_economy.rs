use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::indexer_db::midnight_deposit;
use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 30;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TotalTokensEconomyPayload {
    pub total_amount: String,
}

pub struct TotalTokensEconomyCollector {
    db: DatabaseConnection,
}

impl TotalTokensEconomyCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl MetricCollector for TotalTokensEconomyCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "total-tokens-economy",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            let rows = midnight_deposit::Entity::find()
                .filter(midnight_deposit::Column::Amount.is_not_null())
                .all(&self.db)
                .await
                .with_context(|| "Failed to load midnight_deposit rows")?;

            let mut total_amount: u128 = 0;
            for row in rows {
                let amount = row
                    .amount
                    .as_ref()
                    .with_context(|| "Missing deposit amount")?
                    .parse::<u128>()
                    .with_context(|| "Invalid deposit amount")?;
                total_amount = total_amount
                    .checked_add(amount)
                    .with_context(|| "Deposit amount overflow")?;
            }

            let payload = TotalTokensEconomyPayload {
                total_amount: total_amount.to_string(),
            };

            Ok(vec![MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize total tokens economy payload")?,
            }])
        })
    }
}
