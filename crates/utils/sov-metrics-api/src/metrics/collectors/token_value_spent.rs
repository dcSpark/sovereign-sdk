use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use utoipa::ToSchema;

use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::rollup_state::{self, RollupState};
use crate::metrics::store::{MetricSample, MetricsStore};

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
const MAX_ROWS_PER_COLLECT: i64 = 2_000;
const ROLLUP_STATE_KEY: &str = "token_value_spent_v1";

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TokenValueSpentPayload {
    pub total_amount: String,
    pub total_transactions: u64,
}

#[derive(Debug, FromQueryResult)]
struct TransferAmountRow {
    event_id: i32,
    amount: Option<String>,
}

#[derive(Debug, FromQueryResult)]
struct LatestTransferEventIdRow {
    latest_event_id: Option<i32>,
}

pub struct TokenValueSpentCollector {
    db: DatabaseConnection,
    store: MetricsStore,
    materialized_view_reads_enabled: bool,
    incremental_backfill_enabled: bool,
}

impl TokenValueSpentCollector {
    pub fn new(
        db: DatabaseConnection,
        store: MetricsStore,
        materialized_view_reads_enabled: bool,
        incremental_backfill_enabled: bool,
    ) -> Self {
        Self {
            db,
            store,
            materialized_view_reads_enabled,
            incremental_backfill_enabled,
        }
    }
}

impl MetricCollector for TokenValueSpentCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "token-value-spent",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            let (total_amount, total_transactions) =
                if should_use_materialized_view(&self.db, self.materialized_view_reads_enabled) {
                    super::amount_aggregates::transfer_amount_totals(&self.db)
                        .await
                        .with_context(|| "Failed to aggregate token value spent")?
                } else {
                    collect_incremental_transfer_totals(
                        &self.db,
                        &self.store,
                        self.incremental_backfill_enabled,
                    )
                    .await
                    .with_context(|| "Failed to collect incremental token value spent")?
                };

            let payload = TokenValueSpentPayload {
                total_amount: total_amount.to_string(),
                total_transactions,
            };

            Ok(vec![MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize token value spent payload")?,
            }])
        })
    }
}

fn should_use_materialized_view(db: &DatabaseConnection, enabled: bool) -> bool {
    enabled && db.get_database_backend() == DatabaseBackend::Postgres
}

async fn collect_incremental_transfer_totals(
    db: &DatabaseConnection,
    store: &MetricsStore,
    backfill_enabled: bool,
) -> Result<(u128, u64)> {
    let mut state = match rollup_state::load(db, ROLLUP_STATE_KEY).await? {
        Some(state) => state,
        None if backfill_enabled => {
            info!("Initializing token-value-spent rollup from beginning");
            RollupState::empty()
        }
        None => {
            let latest_event_id = load_latest_transfer_event_id(db).await?;
            let (seed_total_amount, seed_total_transactions) =
                load_seed_from_store(store).await.unwrap_or((0, 0));
            let state = RollupState {
                last_id: i64::from(latest_event_id),
                total_amount: seed_total_amount.to_string(),
                total_transactions: i64::try_from(seed_total_transactions)
                    .context("Persisted token-value-spent seed count does not fit in i64")?,
                ..RollupState::empty()
            };
            rollup_state::save(db, ROLLUP_STATE_KEY, &state).await?;
            info!(
                latest_event_id,
                seed_total_amount,
                seed_total_transactions,
                "Initialized token-value-spent rollup at current transfer tip"
            );
            return Ok((seed_total_amount, seed_total_transactions));
        }
    };

    let mut total_amount = state
        .total_amount
        .parse::<u128>()
        .context("Invalid persisted token-value-spent total amount")?;
    let mut total_transactions = u64::try_from(state.total_transactions)
        .context("Invalid persisted token-value-spent transaction count")?;

    let rows = load_transfer_amount_rows(db, state.last_id, MAX_ROWS_PER_COLLECT).await?;
    if rows.len() as i64 == MAX_ROWS_PER_COLLECT {
        debug!(
            batch_size = MAX_ROWS_PER_COLLECT,
            last_id = state.last_id,
            "Token-value-spent rollup reached batch limit; backlog remains"
        );
    }

    for row in rows {
        if i64::from(row.event_id) > state.last_id {
            state.last_id = i64::from(row.event_id);
        }

        let Some(amount) = row.amount.as_ref() else {
            continue;
        };
        let amount = match amount.parse::<u128>() {
            Ok(amount) => amount,
            Err(error) => {
                warn!(
                    event_id = row.event_id,
                    amount,
                    error = %error,
                    "Skipping invalid transfer amount"
                );
                continue;
            }
        };
        total_amount = total_amount
            .checked_add(amount)
            .context("Token-value-spent amount overflow")?;
        total_transactions = total_transactions
            .checked_add(1)
            .context("Token-value-spent transaction count overflow")?;
    }

    state.total_amount = total_amount.to_string();
    state.total_transactions = i64::try_from(total_transactions)
        .context("Token-value-spent transaction count does not fit in i64")?;
    rollup_state::save(db, ROLLUP_STATE_KEY, &state).await?;

    Ok((total_amount, total_transactions))
}

async fn load_seed_from_store(store: &MetricsStore) -> Option<(u128, u64)> {
    let latest = store.snapshot("token-value-spent").await?.latest?;
    let payload: TokenValueSpentPayload = serde_json::from_value(latest.payload).ok()?;
    let total_amount = payload.total_amount.parse::<u128>().ok()?;
    Some((total_amount, payload.total_transactions))
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
        .context("Failed to query latest transfer event id")?;

    Ok(row.and_then(|row| row.latest_event_id).unwrap_or(0))
}

async fn load_transfer_amount_rows(
    db: &DatabaseConnection,
    last_id: i64,
    batch_size: i64,
) -> Result<Vec<TransferAmountRow>> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            r#"
            SELECT event_id, amount
            FROM midnight_transfer
            WHERE event_id > $1
              AND amount IS NOT NULL
            ORDER BY event_id ASC
            LIMIT $2
            "#
        }
        _ => {
            r#"
            SELECT event_id, amount
            FROM midnight_transfer
            WHERE event_id > ?1
              AND amount IS NOT NULL
            ORDER BY event_id ASC
            LIMIT ?2
            "#
        }
    };

    TransferAmountRow::find_by_statement(Statement::from_sql_and_values(
        backend,
        sql.to_owned(),
        vec![last_id.into(), batch_size.into()],
    ))
    .all(db)
    .await
    .context("Failed to query incremental transfer amounts")
}
