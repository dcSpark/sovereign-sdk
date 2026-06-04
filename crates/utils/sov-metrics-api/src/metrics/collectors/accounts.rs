//! Account metrics collectors for MockMCP-compatible /metrics/* endpoints.
//!
//! These collectors gather:
//! - Total accounts (from fvk_registry)
//! - Sending accounts (unique senders in recent transactions)

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult, PaginatorTrait, Statement,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use utoipa::ToSchema;

use crate::materialized_views::INDEXER_ACCOUNT_TOTALS_VIEW;
use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::rollup_state::{self, RollupState};
use crate::metrics::store::{MetricSample, MetricsStore};

pub const SAMPLE_INTERVAL_SECS: u64 = 5;
const MAX_DISCLOSURE_ROWS_PER_COLLECT: i64 = 2_000;
const TRANSFER_DISCLOSURE_STATE_KEY: &str = "account_transfer_disclosures_v1";
const WITHDRAW_DISCLOSURE_STATE_KEY: &str = "account_withdraw_disclosures_v1";

/// Payload for the accounts metric series.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct AccountsPayload {
    /// Total number of registered accounts (from fvk_registry).
    pub total_accounts: u64,
    /// Number of accounts that sent transactions in the recent window.
    pub sending_accounts: u64,
    /// Total number of disclosure events.
    pub total_disclosure_events: u64,
}

/// Collector for account-related metrics.
///
/// Gathers:
/// - Total accounts from fvk_registry
/// - Sending accounts from recent transactions
/// - Disclosure events count
pub struct AccountsCollector {
    indexer_db: DatabaseConnection,
    store: MetricsStore,
    #[allow(dead_code)]
    da_db: DatabaseConnection,
    /// Window in seconds for "recent" sending accounts.
    sending_window_secs: u64,
    materialized_view_reads_enabled: bool,
    incremental_backfill_enabled: bool,
}

impl AccountsCollector {
    pub fn new(
        indexer_db: DatabaseConnection,
        da_db: DatabaseConnection,
        store: MetricsStore,
        materialized_view_reads_enabled: bool,
        incremental_backfill_enabled: bool,
    ) -> Self {
        Self {
            indexer_db,
            store,
            da_db,
            sending_window_secs: 300, // 5 minutes default
            materialized_view_reads_enabled,
            incremental_backfill_enabled,
        }
    }

    #[allow(dead_code)]
    pub fn with_sending_window(mut self, secs: u64) -> Self {
        self.sending_window_secs = secs;
        self
    }
}

impl MetricCollector for AccountsCollector {
    fn spec(&self) -> MetricSpec {
        MetricSpec {
            name: "accounts",
            interval: Duration::from_secs(SAMPLE_INTERVAL_SECS),
        }
    }

    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>> {
        Box::pin(async move {
            if self.materialized_view_reads_enabled
                && self.indexer_db.get_database_backend() == sea_orm::DatabaseBackend::Postgres
            {
                let payload = load_account_totals_from_mv(&self.indexer_db).await?;
                return Ok(vec![MetricSample {
                    recorded_at_ms: Utc::now().timestamp_millis(),
                    payload: serde_json::to_value(payload)
                        .context("Failed to serialize accounts payload")?,
                }]);
            }

            // Count total accounts from fvk_registry
            let total_accounts = count_fvk_registry_entries(&self.indexer_db).await?;

            // Count unique senders in recent window
            let sending_accounts =
                count_recent_senders(&self.indexer_db, self.sending_window_secs).await?;

            let total_disclosure_events = count_disclosure_events(
                &self.indexer_db,
                &self.store,
                self.incremental_backfill_enabled,
            )
            .await?;

            let payload = AccountsPayload {
                total_accounts,
                sending_accounts,
                total_disclosure_events,
            };

            Ok(vec![MetricSample {
                recorded_at_ms: Utc::now().timestamp_millis(),
                payload: serde_json::to_value(payload)
                    .context("Failed to serialize accounts payload")?,
            }])
        })
    }
}

/// Counts entries in the fvk_registry table.
async fn count_fvk_registry_entries(db: &DatabaseConnection) -> Result<u64> {
    use crate::indexer_db::fvk_registry;

    let count = fvk_registry::Entity::find()
        .count(db)
        .await
        .context("Failed to count fvk_registry entries")?;

    Ok(count)
}

/// Counts unique senders (privacy_sender) in recent transfer/withdraw transactions.
async fn count_recent_senders(db: &DatabaseConnection, window_secs: u64) -> Result<u64> {
    let backend = db.get_database_backend();
    let now = Utc::now();
    let window_start = now - chrono::Duration::seconds(window_secs as i64);

    #[derive(Debug, FromQueryResult)]
    struct CountResult {
        cnt: i64,
    }

    let sql = match backend {
        sea_orm::DatabaseBackend::Postgres => {
            format!(
                r#"
                SELECT COUNT(DISTINCT privacy_sender) as cnt
                FROM (
                    SELECT t.privacy_sender
                    FROM midnight_transfer t
                    JOIN events e ON e.id = t.event_id
                    WHERE t.privacy_sender IS NOT NULL
                      AND e.created_at >= '{}'::timestamp
                    UNION
                    SELECT w.privacy_sender
                    FROM midnight_withdraw w
                    JOIN events e ON e.id = w.event_id
                    WHERE w.privacy_sender IS NOT NULL
                      AND e.created_at >= '{}'::timestamp
                ) senders
                "#,
                window_start.format("%Y-%m-%d %H:%M:%S"),
                window_start.format("%Y-%m-%d %H:%M:%S")
            )
        }
        sea_orm::DatabaseBackend::Sqlite => {
            let window_start_iso = window_start.format("%Y-%m-%dT%H:%M:%S").to_string();
            format!(
                r#"
                SELECT COUNT(DISTINCT privacy_sender) as cnt
                FROM (
                    SELECT t.privacy_sender
                    FROM midnight_transfer t
                    JOIN events e ON e.id = t.event_id
                    WHERE t.privacy_sender IS NOT NULL
                      AND substr(e.created_at, 1, 19) >= '{}'
                    UNION
                    SELECT w.privacy_sender
                    FROM midnight_withdraw w
                    JOIN events e ON e.id = w.event_id
                    WHERE w.privacy_sender IS NOT NULL
                      AND substr(e.created_at, 1, 19) >= '{}'
                ) senders
                "#,
                window_start_iso, window_start_iso
            )
        }
        _ => {
            format!(
                r#"
                SELECT COUNT(DISTINCT privacy_sender) as cnt
                FROM (
                    SELECT t.privacy_sender
                    FROM midnight_transfer t
                    JOIN events e ON e.id = t.event_id
                    WHERE t.privacy_sender IS NOT NULL
                      AND e.created_at >= '{}'
                    UNION
                    SELECT w.privacy_sender
                    FROM midnight_withdraw w
                    JOIN events e ON e.id = w.event_id
                    WHERE w.privacy_sender IS NOT NULL
                      AND e.created_at >= '{}'
                ) senders
                "#,
                window_start.format("%Y-%m-%d %H:%M:%S"),
                window_start.format("%Y-%m-%d %H:%M:%S")
            )
        }
    };

    let stmt = Statement::from_string(backend, sql);
    let result = CountResult::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to count recent senders")?;

    Ok(result.map(|r| r.cnt as u64).unwrap_or(0))
}

async fn count_disclosure_events(
    db: &DatabaseConnection,
    store: &MetricsStore,
    backfill_enabled: bool,
) -> Result<u64> {
    let seed_total_disclosures = load_disclosure_seed_from_store(store).await.unwrap_or(0);
    let transfer_disclosures = collect_disclosure_rollup(
        db,
        TRANSFER_DISCLOSURE_STATE_KEY,
        DisclosureTable::Transfer,
        backfill_enabled,
        seed_total_disclosures,
    )
    .await?;
    let withdraw_disclosures = collect_disclosure_rollup(
        db,
        WITHDRAW_DISCLOSURE_STATE_KEY,
        DisclosureTable::Withdraw,
        backfill_enabled,
        0,
    )
    .await?;

    Ok(transfer_disclosures + withdraw_disclosures)
}

async fn load_disclosure_seed_from_store(store: &MetricsStore) -> Option<u64> {
    let latest = store.snapshot("accounts").await?.latest?;
    let payload: AccountsPayload = serde_json::from_value(latest.payload).ok()?;
    Some(payload.total_disclosure_events)
}

#[derive(Clone, Copy, Debug)]
enum DisclosureTable {
    Transfer,
    Withdraw,
}

#[derive(Debug, FromQueryResult)]
struct DisclosureEventIdRow {
    event_id: i32,
}

#[derive(Debug, FromQueryResult)]
struct LatestDisclosureEventIdRow {
    latest_event_id: Option<i32>,
}

async fn collect_disclosure_rollup(
    db: &DatabaseConnection,
    state_key: &str,
    table: DisclosureTable,
    backfill_enabled: bool,
    seed_total: u64,
) -> Result<u64> {
    let mut state = match rollup_state::load(db, state_key).await? {
        Some(state) => state,
        None if backfill_enabled => {
            info!(state_key, "Initializing disclosure rollup from beginning");
            RollupState::empty()
        }
        None => {
            let latest_event_id = load_latest_disclosure_event_id(db, table).await?;
            let state = RollupState {
                last_id: i64::from(latest_event_id),
                total_transactions: i64::try_from(seed_total)
                    .context("Disclosure seed total does not fit in i64")?,
                ..RollupState::empty()
            };
            rollup_state::save(db, state_key, &state).await?;
            info!(
                state_key,
                latest_event_id, seed_total, "Initialized disclosure rollup at current tip"
            );
            return Ok(seed_total);
        }
    };

    let mut total = u64::try_from(state.total_transactions)
        .context("Invalid persisted disclosure event count")?;
    let rows =
        load_disclosure_rows(db, table, state.last_id, MAX_DISCLOSURE_ROWS_PER_COLLECT).await?;
    if rows.len() as i64 == MAX_DISCLOSURE_ROWS_PER_COLLECT {
        debug!(
            state_key,
            batch_size = MAX_DISCLOSURE_ROWS_PER_COLLECT,
            last_id = state.last_id,
            "Disclosure rollup reached batch limit; backlog remains"
        );
    }

    for row in rows {
        if i64::from(row.event_id) > state.last_id {
            state.last_id = i64::from(row.event_id);
        }
        total = total
            .checked_add(1)
            .context("Disclosure event count overflow")?;
    }

    state.total_transactions =
        i64::try_from(total).context("Disclosure event count does not fit in i64")?;
    rollup_state::save(db, state_key, &state).await?;

    Ok(total)
}

async fn load_latest_disclosure_event_id(
    db: &DatabaseConnection,
    table: DisclosureTable,
) -> Result<i32> {
    let backend = db.get_database_backend();
    let table_name = match table {
        DisclosureTable::Transfer => "midnight_transfer",
        DisclosureTable::Withdraw => "midnight_withdraw",
    };
    let stmt = Statement::from_string(
        backend,
        format!(
            "
            SELECT MAX(event_id) AS latest_event_id
            FROM {table_name}
            WHERE view_attestations IS NOT NULL
            "
        ),
    );

    let row = LatestDisclosureEventIdRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query latest disclosure event id")?;

    Ok(row.and_then(|row| row.latest_event_id).unwrap_or(0))
}

async fn load_disclosure_rows(
    db: &DatabaseConnection,
    table: DisclosureTable,
    last_id: i64,
    batch_size: i64,
) -> Result<Vec<DisclosureEventIdRow>> {
    let backend = db.get_database_backend();
    let table_name = match table {
        DisclosureTable::Transfer => "midnight_transfer",
        DisclosureTable::Withdraw => "midnight_withdraw",
    };
    let sql = match backend {
        sea_orm::DatabaseBackend::Postgres => {
            format!(
                r#"
                SELECT event_id
                FROM {table_name}
                WHERE event_id > $1
                  AND view_attestations IS NOT NULL
                ORDER BY event_id ASC
                LIMIT $2
                "#
            )
        }
        _ => {
            format!(
                r#"
                SELECT event_id
                FROM {table_name}
                WHERE event_id > ?1
                  AND view_attestations IS NOT NULL
                ORDER BY event_id ASC
                LIMIT ?2
                "#
            )
        }
    };

    DisclosureEventIdRow::find_by_statement(Statement::from_sql_and_values(
        backend,
        sql,
        vec![last_id.into(), batch_size.into()],
    ))
    .all(db)
    .await
    .context("Failed to query disclosure event rows")
}

#[derive(Debug, FromQueryResult)]
struct AccountTotalsMvRow {
    total_accounts: i64,
    sending_accounts_5m: i64,
    total_disclosure_events: i64,
}

async fn load_account_totals_from_mv(db: &DatabaseConnection) -> Result<AccountsPayload> {
    let stmt = Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "
            SELECT total_accounts, sending_accounts_5m, total_disclosure_events
            FROM {INDEXER_ACCOUNT_TOTALS_VIEW}
            WHERE id = 1
            "
        ),
    );

    let row = AccountTotalsMvRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query account totals materialized view")?
        .context("Missing account totals row in materialized view")?;

    let total_accounts = u64::try_from(row.total_accounts)
        .with_context(|| format!("Negative total_accounts in MV: {}", row.total_accounts))?;
    let sending_accounts = u64::try_from(row.sending_accounts_5m).with_context(|| {
        format!(
            "Negative sending_accounts_5m in MV: {}",
            row.sending_accounts_5m
        )
    })?;
    let total_disclosure_events =
        u64::try_from(row.total_disclosure_events).with_context(|| {
            format!(
                "Negative total_disclosure_events in MV: {}",
                row.total_disclosure_events
            )
        })?;

    Ok(AccountsPayload {
        total_accounts,
        sending_accounts,
        total_disclosure_events,
    })
}
