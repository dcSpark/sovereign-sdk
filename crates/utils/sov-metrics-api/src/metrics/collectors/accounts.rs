//! Account metrics collectors for MockMCP-compatible /metrics/* endpoints.
//!
//! These collectors gather:
//! - Total accounts (from fvk_registry)
//! - Sending accounts (unique senders in recent transactions)

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult, PaginatorTrait,
    QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::materialized_views::INDEXER_ACCOUNT_TOTALS_VIEW;
use crate::metrics::collector::{BoxFuture, MetricCollector, MetricSpec};
use crate::metrics::store::MetricSample;

pub const SAMPLE_INTERVAL_SECS: u64 = 5;

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
    #[allow(dead_code)]
    da_db: DatabaseConnection,
    /// Window in seconds for "recent" sending accounts.
    sending_window_secs: u64,
}

impl AccountsCollector {
    pub fn new(indexer_db: DatabaseConnection, da_db: DatabaseConnection) -> Self {
        Self {
            indexer_db,
            da_db,
            sending_window_secs: 300, // 5 minutes default
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
            if self.indexer_db.get_database_backend() == sea_orm::DatabaseBackend::Postgres {
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

            // Count disclosure events (view_attestations in transfers/withdraws)
            let total_disclosure_events = count_disclosure_events(&self.indexer_db).await?;

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

/// Counts disclosure events (transfers/withdraws with view_attestations).
async fn count_disclosure_events(db: &DatabaseConnection) -> Result<u64> {
    use crate::indexer_db::{midnight_transfer, midnight_withdraw};

    // Count transfers with view_attestations
    let transfer_disclosures = midnight_transfer::Entity::find()
        .filter(midnight_transfer::Column::ViewAttestations.is_not_null())
        .count(db)
        .await
        .context("Failed to count transfer disclosures")?;

    // Count withdraws with view_attestations
    let withdraw_disclosures = midnight_withdraw::Entity::find()
        .filter(midnight_withdraw::Column::ViewAttestations.is_not_null())
        .count(db)
        .await
        .context("Failed to count withdraw disclosures")?;

    Ok(transfer_disclosures + withdraw_disclosures)
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
