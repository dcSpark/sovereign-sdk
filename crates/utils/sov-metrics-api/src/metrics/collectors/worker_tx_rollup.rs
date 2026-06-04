use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::failed_transactions::FailedTransactionsPayload;
use super::total_transactions::TotalTransactionsPayload;
use crate::metrics::rollup_state::{self, RollupState};
use crate::metrics::store::MetricsStore;

const MAX_ROWS_PER_COLLECT: i64 = 2_000;
const ROLLUP_STATE_KEY: &str = "worker_tx_totals_v1";
static WORKER_TX_ROLLUP_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Copy)]
pub struct WorkerTxTotals {
    pub total_transactions: u64,
    pub failed_transactions: u64,
}

#[derive(Debug, FromQueryResult)]
struct WorkerTxRow {
    id: i32,
    transaction_state: String,
    created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, FromQueryResult)]
struct LatestWorkerTxRow {
    id: i32,
    created_at: chrono::DateTime<Utc>,
}

pub async fn collect(
    db: &DatabaseConnection,
    store: &MetricsStore,
    backfill_enabled: bool,
) -> Result<WorkerTxTotals> {
    let _guard = WORKER_TX_ROLLUP_LOCK.lock().await;

    let mut state = match rollup_state::load(db, ROLLUP_STATE_KEY).await? {
        Some(state) => state,
        None if backfill_enabled => {
            info!("Initializing worker transaction totals rollup from beginning");
            RollupState::empty()
        }
        None => {
            let latest = load_latest_completed_tx(db).await?;
            let seed = load_seed_from_store(store).await;
            let state = latest
                .map(|row| RollupState {
                    last_id: i64::from(row.id),
                    last_timestamp_ms: Some(row.created_at.timestamp_millis()),
                    total_transactions: i64::try_from(seed.total_transactions).unwrap_or(i64::MAX),
                    failed_transactions: i64::try_from(seed.failed_transactions)
                        .unwrap_or(i64::MAX),
                    ..RollupState::empty()
                })
                .unwrap_or_else(|| RollupState {
                    total_transactions: i64::try_from(seed.total_transactions).unwrap_or(i64::MAX),
                    failed_transactions: i64::try_from(seed.failed_transactions)
                        .unwrap_or(i64::MAX),
                    ..RollupState::empty()
                });
            rollup_state::save(db, ROLLUP_STATE_KEY, &state).await?;
            info!(
                last_id = state.last_id,
                last_timestamp_ms = state.last_timestamp_ms,
                seed_total_transactions = seed.total_transactions,
                seed_failed_transactions = seed.failed_transactions,
                "Initialized worker transaction totals rollup at current completed tx tip"
            );
            return Ok(seed);
        }
    };

    let mut total_transactions = u64::try_from(state.total_transactions)
        .context("Invalid persisted worker total transaction count")?;
    let mut failed_transactions = u64::try_from(state.failed_transactions)
        .context("Invalid persisted worker failed transaction count")?;

    let rows = load_completed_tx_rows(db, &state, MAX_ROWS_PER_COLLECT).await?;
    if rows.len() as i64 == MAX_ROWS_PER_COLLECT {
        debug!(
            batch_size = MAX_ROWS_PER_COLLECT,
            last_id = state.last_id,
            last_timestamp_ms = state.last_timestamp_ms,
            "Worker transaction totals rollup reached batch limit; backlog remains"
        );
    }

    for row in rows {
        state.last_id = i64::from(row.id);
        state.last_timestamp_ms = Some(row.created_at.timestamp_millis());

        total_transactions = total_transactions
            .checked_add(1)
            .context("Worker total transaction count overflow")?;
        if row.transaction_state == "rejected" {
            failed_transactions = failed_transactions
                .checked_add(1)
                .context("Worker failed transaction count overflow")?;
        }
    }

    state.total_transactions = i64::try_from(total_transactions)
        .context("Worker total transaction count does not fit in i64")?;
    state.failed_transactions = i64::try_from(failed_transactions)
        .context("Worker failed transaction count does not fit in i64")?;
    rollup_state::save(db, ROLLUP_STATE_KEY, &state).await?;

    Ok(WorkerTxTotals {
        total_transactions,
        failed_transactions,
    })
}

async fn load_seed_from_store(store: &MetricsStore) -> WorkerTxTotals {
    let total_from_total_series = match store.snapshot("total-transactions").await {
        Some(series) => series.latest.and_then(|sample| {
            serde_json::from_value::<TotalTransactionsPayload>(sample.payload)
                .ok()
                .map(|payload| payload.total_transactions)
        }),
        None => None,
    };

    let failed_payload = match store.snapshot("failed-transactions-rate").await {
        Some(series) => series.latest.and_then(|sample| {
            serde_json::from_value::<FailedTransactionsPayload>(sample.payload).ok()
        }),
        None => None,
    };

    WorkerTxTotals {
        total_transactions: total_from_total_series
            .or_else(|| {
                failed_payload
                    .as_ref()
                    .map(|payload| payload.total_transactions)
            })
            .unwrap_or(0),
        failed_transactions: failed_payload
            .map(|payload| payload.failed_transactions)
            .unwrap_or(0),
    }
}

async fn load_latest_completed_tx(db: &DatabaseConnection) -> Result<Option<LatestWorkerTxRow>> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            r#"
            SELECT id, created_at
            FROM worker_verified_transactions
            WHERE transaction_state IN ('accepted', 'rejected')
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#
        }
        _ => {
            r#"
            SELECT id, created_at
            FROM worker_verified_transactions
            WHERE transaction_state IN ('accepted', 'rejected')
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#
        }
    };

    LatestWorkerTxRow::find_by_statement(Statement::from_string(backend, sql.to_owned()))
        .one(db)
        .await
        .context("Failed to query latest completed worker transaction")
}

async fn load_completed_tx_rows(
    db: &DatabaseConnection,
    state: &RollupState,
    batch_size: i64,
) -> Result<Vec<WorkerTxRow>> {
    let backend = db.get_database_backend();
    let Some(last_timestamp_ms) = state.last_timestamp_ms else {
        let sql = match backend {
            DatabaseBackend::Postgres => {
                r#"
                SELECT id, transaction_state, created_at
                FROM worker_verified_transactions
                WHERE transaction_state IN ('accepted', 'rejected')
                ORDER BY created_at ASC, id ASC
                LIMIT $1
                "#
            }
            _ => {
                r#"
                SELECT id, transaction_state, created_at
                FROM worker_verified_transactions
                WHERE transaction_state IN ('accepted', 'rejected')
                ORDER BY created_at ASC, id ASC
                LIMIT ?1
                "#
            }
        };

        return WorkerTxRow::find_by_statement(Statement::from_sql_and_values(
            backend,
            sql.to_owned(),
            [batch_size.into()],
        ))
        .all(db)
        .await
        .context("Failed to query completed worker transactions");
    };

    let last_created_at = chrono::DateTime::<Utc>::from_timestamp_millis(last_timestamp_ms)
        .context("Invalid persisted worker transaction timestamp")?;
    let sql = match backend {
        DatabaseBackend::Postgres => {
            r#"
            SELECT id, transaction_state, created_at
            FROM worker_verified_transactions
            WHERE transaction_state IN ('accepted', 'rejected')
              AND (
                    created_at > $1
                 OR (created_at = $1 AND id > $2)
              )
            ORDER BY created_at ASC, id ASC
            LIMIT $3
            "#
        }
        _ => {
            r#"
            SELECT id, transaction_state, created_at
            FROM worker_verified_transactions
            WHERE transaction_state IN ('accepted', 'rejected')
              AND (
                    created_at > ?1
                 OR (created_at = ?1 AND id > ?2)
              )
            ORDER BY created_at ASC, id ASC
            LIMIT ?3
            "#
        }
    };

    WorkerTxRow::find_by_statement(Statement::from_sql_and_values(
        backend,
        sql.to_owned(),
        vec![
            last_created_at.into(),
            state.last_id.into(),
            batch_size.into(),
        ],
    ))
    .all(db)
    .await
    .context("Failed to query completed worker transactions")
}
