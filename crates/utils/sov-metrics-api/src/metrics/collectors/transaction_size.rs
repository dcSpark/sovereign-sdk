use std::time::Duration;

use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
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
    last_seen_event_id: Mutex<Option<i32>>,
    retention_secs: i64,
    backfill_enabled: bool,
}

impl TransactionSizeCollector {
    pub fn new(db: DatabaseConnection, retention_secs: u64, backfill_enabled: bool) -> Self {
        Self {
            db,
            last_seen_event_id: Mutex::new(None),
            retention_secs: i64::try_from(retention_secs).unwrap_or(i64::MAX),
            backfill_enabled,
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
            let retention_cutoff =
                chrono::Utc::now() - chrono::Duration::seconds(self.retention_secs);
            let last_seen = match *guard {
                Some(last_seen) => last_seen,
                None if self.backfill_enabled => {
                    let initial_event_id = load_retention_start_event_id(&self.db, retention_cutoff)
                        .await
                        .context("Failed to initialize transaction-size backfill cursor")?;
                    *guard = Some(initial_event_id);
                    info!(
                        initial_event_id,
                        "Transaction size collector initialized for retention-window backfill"
                    );
                    initial_event_id
                }
                None => {
                    let latest_event_id = load_latest_transfer_event_id(&self.db)
                        .await
                        .context("Failed to initialize transaction-size cursor")?;
                    *guard = Some(latest_event_id);
                    info!(
                        latest_event_id,
                        "Transaction size collector initialized without historical backfill"
                    );
                    return Ok(Vec::new());
                }
            };

            let rows = load_transaction_size_rows(
                &self.db,
                last_seen,
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

            *guard = Some(latest_seen);

            Ok(samples)
        })
    }
}

#[derive(Debug, FromQueryResult)]
struct LatestTransferEventIdRow {
    latest_event_id: Option<i32>,
}

#[derive(Debug, FromQueryResult)]
struct RetentionStartEventIdRow {
    first_event_id: Option<i32>,
}

async fn load_latest_transfer_event_id(db: &DatabaseConnection) -> Result<i32> {
    let backend = db.get_database_backend();
    let stmt = Statement::from_string(
        backend,
        "
        SELECT MAX(event_id) AS latest_event_id
        FROM midnight_transfer
        "
        .to_owned(),
    );

    let row = LatestTransferEventIdRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query latest midnight_transfer event id")?;

    Ok(row.and_then(|row| row.latest_event_id).unwrap_or(0))
}

async fn load_retention_start_event_id(
    db: &DatabaseConnection,
    retention_cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<i32> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            r#"
            SELECT MIN(id) AS first_event_id
            FROM events
            WHERE created_at >= $1
            "#
        }
        DatabaseBackend::Sqlite => {
            r#"
            SELECT MIN(id) AS first_event_id
            FROM events
            WHERE created_at >= ?1
            "#
        }
        _ => {
            r#"
            SELECT MIN(id) AS first_event_id
            FROM events
            WHERE created_at >= ?1
            "#
        }
    };

    let stmt = Statement::from_sql_and_values(backend, sql.to_owned(), [retention_cutoff.into()]);

    let row = RetentionStartEventIdRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query transaction-size retention start event id")?;

    match row.and_then(|row| row.first_event_id) {
        Some(first_event_id) => Ok(first_event_id.saturating_sub(1)),
        None => load_latest_transfer_event_id(db).await,
    }
}

async fn load_transaction_size_rows(
    db: &DatabaseConnection,
    last_seen: i32,
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
              AND t.amount IS NOT NULL
            ORDER BY t.event_id ASC
            LIMIT $2
            "#
        }
        DatabaseBackend::Sqlite => {
            r#"
            SELECT t.event_id, t.amount, e.created_at
            FROM midnight_transfer t
            INNER JOIN events e ON e.id = t.event_id
            WHERE t.event_id > ?1
              AND t.amount IS NOT NULL
            ORDER BY t.event_id ASC
            LIMIT ?2
            "#
        }
        _ => {
            r#"
            SELECT t.event_id, t.amount, e.created_at
            FROM midnight_transfer t
            INNER JOIN events e ON e.id = t.event_id
            WHERE t.event_id > ?1
              AND t.amount IS NOT NULL
            ORDER BY t.event_id ASC
            LIMIT ?2
            "#
        }
    };

    let stmt = Statement::from_sql_and_values(
        backend,
        sql.to_owned(),
        vec![last_seen.into(), batch_size.into()],
    );

    TransactionSizeRow::find_by_statement(stmt)
        .all(db)
        .await
        .context("Failed to query transaction size rows")
}
