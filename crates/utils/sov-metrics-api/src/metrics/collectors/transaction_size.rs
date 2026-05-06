use std::time::Duration;

use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, warn};
use utoipa::ToSchema;

use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
const MAX_ROWS_PER_COLLECT: i64 = 2_000;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TransactionSizePayload {
    pub amount: String,
}

#[derive(Debug, FromQueryResult)]
struct TransactionSizeRow {
    event_id: i32,
    amount: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

pub struct TransactionSizeCollector {
    db: DatabaseConnection,
    last_seen_event_id: Mutex<i32>,
    retention_secs: i64,
}

impl TransactionSizeCollector {
    pub fn new(db: DatabaseConnection, retention_secs: u64) -> Self {
        Self {
            db,
            last_seen_event_id: Mutex::new(0),
            retention_secs: i64::try_from(retention_secs).unwrap_or(i64::MAX),
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
            let retention_cutoff =
                chrono::Utc::now() - chrono::Duration::seconds(self.retention_secs);

            let rows = load_transaction_size_rows(
                &self.db,
                last_seen,
                retention_cutoff,
                MAX_ROWS_PER_COLLECT,
            )
            .await
            .with_context(|| "Failed to load midnight_transfer rows")?;

            if rows.len() as i64 == MAX_ROWS_PER_COLLECT {
                debug!(
                    batch_size = MAX_ROWS_PER_COLLECT,
                    last_seen_event_id = last_seen,
                    "Transaction size collector reached batch limit; backlog remains"
                );
            }

            let mut samples = Vec::with_capacity(rows.len());
            let mut latest_seen = last_seen;

            for row in rows {
                if row.event_id > latest_seen {
                    latest_seen = row.event_id;
                }

                let amount = match row.amount.as_ref() {
                    Some(amount) => amount,
                    None => continue,
                };
                if amount.parse::<u128>().is_err() {
                    warn!(event_id = row.event_id, amount, "Invalid transfer amount");
                    continue;
                }

                let payload = TransactionSizePayload {
                    amount: amount.clone(),
                };

                samples.push(MetricSample {
                    recorded_at_ms: row.created_at.timestamp_millis(),
                    payload: serde_json::to_value(payload)
                        .context("Failed to serialize transaction size payload")?,
                });
            }

            *guard = latest_seen;

            Ok(samples)
        })
    }
}

async fn load_transaction_size_rows(
    db: &DatabaseConnection,
    last_seen: i32,
    retention_cutoff: chrono::DateTime<chrono::Utc>,
    batch_size: i64,
) -> Result<Vec<TransactionSizeRow>> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            r#"
            SELECT t.event_id, t.amount, e.created_at
            FROM midnight_transfer t
            INNER JOIN events e ON e.id = t.event_id
            WHERE t.event_id > $1
              AND e.created_at >= $2
              AND t.amount IS NOT NULL
            ORDER BY t.event_id ASC
            LIMIT $3
            "#
        }
        DatabaseBackend::Sqlite => {
            r#"
            SELECT t.event_id, t.amount, e.created_at
            FROM midnight_transfer t
            INNER JOIN events e ON e.id = t.event_id
            WHERE t.event_id > ?1
              AND e.created_at >= ?2
              AND t.amount IS NOT NULL
            ORDER BY t.event_id ASC
            LIMIT ?3
            "#
        }
        _ => {
            r#"
            SELECT t.event_id, t.amount, e.created_at
            FROM midnight_transfer t
            INNER JOIN events e ON e.id = t.event_id
            WHERE t.event_id > ?1
              AND e.created_at >= ?2
              AND t.amount IS NOT NULL
            ORDER BY t.event_id ASC
            LIMIT ?3
            "#
        }
    };

    let stmt = Statement::from_sql_and_values(
        backend,
        sql.to_owned(),
        vec![
            last_seen.into(),
            retention_cutoff.into(),
            batch_size.into(),
        ],
    );

    TransactionSizeRow::find_by_statement(stmt)
        .all(db)
        .await
        .context("Failed to query transaction size rows")
}
