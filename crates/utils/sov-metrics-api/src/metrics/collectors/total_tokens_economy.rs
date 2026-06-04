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

pub const SAMPLE_INTERVAL_SECS: u64 = 30;
const MAX_ROWS_PER_COLLECT: i64 = 2_000;
const ROLLUP_STATE_KEY: &str = "total_tokens_economy_v1";

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TotalTokensEconomyPayload {
    pub total_amount: String,
}

#[derive(Debug, FromQueryResult)]
struct DepositAmountRow {
    event_id: i32,
    amount: Option<String>,
}

#[derive(Debug, FromQueryResult)]
struct LatestDepositEventIdRow {
    latest_event_id: Option<i32>,
}

pub struct TotalTokensEconomyCollector {
    db: DatabaseConnection,
    store: MetricsStore,
    materialized_view_reads_enabled: bool,
    incremental_backfill_enabled: bool,
}

impl TotalTokensEconomyCollector {
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

impl MetricCollector for TotalTokensEconomyCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "total-tokens-economy",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            let total_amount =
                if should_use_materialized_view(&self.db, self.materialized_view_reads_enabled) {
                    super::amount_aggregates::deposit_total_amount(&self.db)
                        .await
                        .with_context(|| "Failed to aggregate total tokens economy")?
                } else {
                    collect_incremental_deposit_total(
                        &self.db,
                        &self.store,
                        self.incremental_backfill_enabled,
                    )
                    .await
                    .with_context(|| "Failed to collect incremental total tokens economy")?
                };

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

fn should_use_materialized_view(db: &DatabaseConnection, enabled: bool) -> bool {
    enabled && db.get_database_backend() == DatabaseBackend::Postgres
}

async fn collect_incremental_deposit_total(
    db: &DatabaseConnection,
    store: &MetricsStore,
    backfill_enabled: bool,
) -> Result<u128> {
    let mut state = match rollup_state::load(db, ROLLUP_STATE_KEY).await? {
        Some(state) => state,
        None if backfill_enabled => {
            info!("Initializing total-tokens-economy rollup from beginning");
            RollupState::empty()
        }
        None => {
            let latest_event_id = load_latest_deposit_event_id(db).await?;
            let seed_total_amount = load_seed_from_store(store).await.unwrap_or(0);
            let state = RollupState {
                last_id: i64::from(latest_event_id),
                total_amount: seed_total_amount.to_string(),
                ..RollupState::empty()
            };
            rollup_state::save(db, ROLLUP_STATE_KEY, &state).await?;
            info!(
                latest_event_id,
                seed_total_amount, "Initialized total-tokens-economy rollup at current deposit tip"
            );
            return Ok(seed_total_amount);
        }
    };

    let mut total_amount = state
        .total_amount
        .parse::<u128>()
        .context("Invalid persisted total-tokens-economy amount")?;

    let rows = load_deposit_amount_rows(db, state.last_id, MAX_ROWS_PER_COLLECT).await?;
    if rows.len() as i64 == MAX_ROWS_PER_COLLECT {
        debug!(
            batch_size = MAX_ROWS_PER_COLLECT,
            last_id = state.last_id,
            "Total-tokens-economy rollup reached batch limit; backlog remains"
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
                    "Skipping invalid deposit amount"
                );
                continue;
            }
        };
        total_amount = total_amount
            .checked_add(amount)
            .context("Total-tokens-economy amount overflow")?;
    }

    state.total_amount = total_amount.to_string();
    rollup_state::save(db, ROLLUP_STATE_KEY, &state).await?;

    Ok(total_amount)
}

async fn load_seed_from_store(store: &MetricsStore) -> Option<u128> {
    let latest = store.snapshot("total-tokens-economy").await?.latest?;
    let payload: TotalTokensEconomyPayload = serde_json::from_value(latest.payload).ok()?;
    payload.total_amount.parse::<u128>().ok()
}

async fn load_latest_deposit_event_id(db: &DatabaseConnection) -> Result<i32> {
    let backend = db.get_database_backend();
    let stmt = Statement::from_string(
        backend,
        "
        SELECT MAX(event_id) AS latest_event_id
        FROM midnight_deposit
        "
        .to_owned(),
    );

    let row = LatestDepositEventIdRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query latest deposit event id")?;

    Ok(row.and_then(|row| row.latest_event_id).unwrap_or(0))
}

async fn load_deposit_amount_rows(
    db: &DatabaseConnection,
    last_id: i64,
    batch_size: i64,
) -> Result<Vec<DepositAmountRow>> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            r#"
            SELECT event_id, amount
            FROM midnight_deposit
            WHERE event_id > $1
              AND amount IS NOT NULL
            ORDER BY event_id ASC
            LIMIT $2
            "#
        }
        _ => {
            r#"
            SELECT event_id, amount
            FROM midnight_deposit
            WHERE event_id > ?1
              AND amount IS NOT NULL
            ORDER BY event_id ASC
            LIMIT ?2
            "#
        }
    };

    DepositAmountRow::find_by_statement(Statement::from_sql_and_values(
        backend,
        sql.to_owned(),
        vec![last_id.into(), batch_size.into()],
    ))
    .all(db)
    .await
    .context("Failed to query incremental deposit amounts")
}
