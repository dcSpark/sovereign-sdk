use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
pub const WINDOW_SECONDS: u64 = SAMPLE_INTERVAL_SECS;
pub const RETENTION_SECONDS: u64 = 300;
pub const MAX_SAMPLES: usize = (RETENTION_SECONDS / SAMPLE_INTERVAL_SECS) as usize;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TpsPayload {
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub completed_tx_count: u64,
    pub peak_tps: u64,
}

pub struct TpsCollector {
    db: DatabaseConnection,
}

impl TpsCollector {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl MetricCollector for TpsCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "tps",
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

            let rows = Entity::find()
                .filter(Column::CreatedAt.gte(window_start))
                .filter(Column::CreatedAt.lt(window_end))
                .all(&self.db)
                .await
                .with_context(|| "Failed to query worker_verified_transactions")?;

            let mut per_second: HashMap<i64, u64> = HashMap::new();
            for row in rows.iter() {
                let second = row.created_at.timestamp();
                *per_second.entry(second).or_insert(0) += 1;
            }

            let peak_tps = per_second.values().copied().max().unwrap_or(0);
            let payload = TpsPayload {
                window_start_ms: window_start.timestamp_millis(),
                window_end_ms: window_end.timestamp_millis(),
                completed_tx_count: rows.len() as u64,
                peak_tps,
            };

            Ok(MetricSample {
                recorded_at_ms: window_end.timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize TPS payload")?,
            })
        })
    }
}
