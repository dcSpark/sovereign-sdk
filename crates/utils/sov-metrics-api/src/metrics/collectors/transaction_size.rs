use std::time::Duration;

use anyhow::{Context, Result};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, JoinType, QueryFilter, QueryOrder, QuerySelect,
    RelationTrait,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;
use utoipa::ToSchema;

use crate::indexer_db::{self, midnight_transfer};
use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TransactionSizePayload {
    pub amount: String,
}

pub struct TransactionSizeCollector {
    db: DatabaseConnection,
    last_seen_event_id: Mutex<i32>,
}

impl TransactionSizeCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            last_seen_event_id: Mutex::new(0),
        }
    }
}

impl MetricCollector for TransactionSizeCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "transaction-size",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            let mut guard = self.last_seen_event_id.lock().await;
            let last_seen = *guard;

            let rows = midnight_transfer::Entity::find()
                .filter(midnight_transfer::Column::EventId.gt(last_seen))
                .filter(midnight_transfer::Column::Amount.is_not_null())
                .join(
                    JoinType::InnerJoin,
                    midnight_transfer::Relation::Events.def(),
                )
                .order_by_asc(midnight_transfer::Column::EventId)
                .select_also(indexer_db::Entity)
                .all(&self.db)
                .await
                .with_context(|| "Failed to load midnight_transfer rows")?;

            let mut samples = Vec::with_capacity(rows.len());
            let mut latest_seen = last_seen;

            for (transfer, event) in rows {
                if transfer.event_id > latest_seen {
                    latest_seen = transfer.event_id;
                }
                let event = match event {
                    Some(event) => event,
                    None => {
                        warn!(event_id = transfer.event_id, "Missing event for transfer");
                        continue;
                    }
                };

                let amount = match transfer.amount.as_ref() {
                    Some(amount) => amount,
                    None => continue,
                };
                if amount.parse::<u128>().is_err() {
                    warn!(
                        event_id = transfer.event_id,
                        amount, "Invalid transfer amount"
                    );
                    continue;
                }

                let payload = TransactionSizePayload {
                    amount: amount.clone(),
                };

                samples.push(MetricSample {
                    recorded_at_ms: event.created_at.timestamp_millis(),
                    payload: serde_json::to_value(payload)
                        .context("Failed to serialize transaction size payload")?,
                });
            }

            *guard = latest_seen;

            Ok(samples)
        })
    }
}
