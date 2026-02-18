use std::time::Duration;

use anyhow::{Context, Result};
use sea_orm::sqlx::{self, Row};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use tokio::time::interval;
use tracing::{debug, info, warn};

pub const INDEXER_TRANSFER_TOTALS_VIEW: &str = "mv_metrics_transfer_totals";
pub const INDEXER_DEPOSIT_TOTALS_VIEW: &str = "mv_metrics_deposit_totals";
pub const INDEXER_TRANSFER_STATS_24H_VIEW: &str = "mv_metrics_transfer_stats_24h";
pub const INDEXER_ACCOUNT_TOTALS_VIEW: &str = "mv_metrics_account_totals";
pub const DA_WORKER_TX_TOTALS_VIEW: &str = "mv_metrics_worker_tx_totals";

const MATERIALIZED_VIEW_VERSION_TABLE: &str = "metrics_materialized_view_versions";

#[derive(Clone, Copy, Debug)]
struct ViewSpec {
    view_name: &'static str,
    version: i32,
    create_sql: &'static str,
    create_unique_index_sql: &'static str,
    refresh_interval_secs: u64,
    stale_after_secs: i64,
    advisory_lock_key: i64,
}

const INDEXER_VIEW_SPECS: &[ViewSpec] = &[
    ViewSpec {
        view_name: INDEXER_TRANSFER_TOTALS_VIEW,
        version: 1,
        create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_transfer_totals AS
SELECT
    1::int AS id,
    COALESCE(
        SUM(
            CASE
                WHEN amount ~ '^[0-9]+$' THEN CAST(amount AS NUMERIC)
                ELSE 0
            END
        ),
        0
    )::text AS total_amount,
    COUNT(*) FILTER (WHERE amount ~ '^[0-9]+$')::bigint AS total_transactions,
    NOW()::timestamptz AS refreshed_at
FROM midnight_transfer
"#,
        create_unique_index_sql: r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_metrics_transfer_totals_id
    ON mv_metrics_transfer_totals (id)
"#,
        refresh_interval_secs: 2,
        stale_after_secs: 5,
        advisory_lock_key: 730_001,
    },
    ViewSpec {
        view_name: INDEXER_DEPOSIT_TOTALS_VIEW,
        version: 1,
        create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_deposit_totals AS
SELECT
    1::int AS id,
    COALESCE(
        SUM(
            CASE
                WHEN amount ~ '^[0-9]+$' THEN CAST(amount AS NUMERIC)
                ELSE 0
            END
        ),
        0
    )::text AS total_amount,
    NOW()::timestamptz AS refreshed_at
FROM midnight_deposit
"#,
        create_unique_index_sql: r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_metrics_deposit_totals_id
    ON mv_metrics_deposit_totals (id)
"#,
        refresh_interval_secs: 30,
        stale_after_secs: 45,
        advisory_lock_key: 730_002,
    },
    ViewSpec {
        view_name: INDEXER_TRANSFER_STATS_24H_VIEW,
        version: 1,
        create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_transfer_stats_24h AS
WITH windowed AS (
    SELECT
        CAST(mt.amount AS numeric) AS amount
    FROM midnight_transfer mt
    INNER JOIN events ev ON ev.id = mt.event_id
    WHERE mt.amount IS NOT NULL
      AND mt.amount ~ '^[0-9]+$'
      AND ev.created_at >= NOW() - INTERVAL '24 hours'
      AND ev.created_at <= NOW()
)
SELECT
    1::int AS id,
    AVG(windowed.amount)::double precision AS average_amount,
    COALESCE(SUM(windowed.amount), 0)::text AS delta_amount,
    COUNT(*)::bigint AS delta_transactions,
    percentile_cont(0.5) WITHIN GROUP (ORDER BY windowed.amount)::double precision AS median_amount,
    (NOW() - INTERVAL '24 hours')::timestamptz AS window_start,
    NOW()::timestamptz AS window_end,
    NOW()::timestamptz AS refreshed_at
FROM windowed
"#,
        create_unique_index_sql: r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_metrics_transfer_stats_24h_id
    ON mv_metrics_transfer_stats_24h (id)
"#,
        refresh_interval_secs: 15,
        stale_after_secs: 20,
        advisory_lock_key: 730_003,
    },
    ViewSpec {
        view_name: INDEXER_ACCOUNT_TOTALS_VIEW,
        version: 1,
        create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_account_totals AS
WITH recent_senders AS (
    SELECT t.privacy_sender
    FROM midnight_transfer t
    JOIN events e ON e.id = t.event_id
    WHERE t.privacy_sender IS NOT NULL
      AND e.created_at >= NOW() - INTERVAL '5 minutes'
    UNION
    SELECT w.privacy_sender
    FROM midnight_withdraw w
    JOIN events e ON e.id = w.event_id
    WHERE w.privacy_sender IS NOT NULL
      AND e.created_at >= NOW() - INTERVAL '5 minutes'
)
SELECT
    1::int AS id,
    (SELECT COUNT(*)::bigint FROM fvk_registry) AS total_accounts,
    (
        (SELECT COUNT(*)::bigint FROM midnight_transfer WHERE view_attestations IS NOT NULL)
        +
        (SELECT COUNT(*)::bigint FROM midnight_withdraw WHERE view_attestations IS NOT NULL)
    ) AS total_disclosure_events,
    (SELECT COUNT(*)::bigint FROM recent_senders) AS sending_accounts_5m,
    NOW()::timestamptz AS refreshed_at
"#,
        create_unique_index_sql: r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_metrics_account_totals_id
    ON mv_metrics_account_totals (id)
"#,
        refresh_interval_secs: 15,
        stale_after_secs: 20,
        advisory_lock_key: 730_004,
    },
];

const DA_VIEW_SPECS: &[ViewSpec] = &[ViewSpec {
    view_name: DA_WORKER_TX_TOTALS_VIEW,
    version: 1,
    create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_worker_tx_totals AS
SELECT
    1::int AS id,
    COUNT(*) FILTER (WHERE transaction_state IN ('accepted', 'rejected'))::bigint AS total_transactions,
    COUNT(*) FILTER (WHERE transaction_state = 'rejected')::bigint AS failed_transactions,
    NOW()::timestamptz AS refreshed_at
FROM worker_verified_transactions
"#,
    create_unique_index_sql: r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_metrics_worker_tx_totals_id
    ON mv_metrics_worker_tx_totals (id)
"#,
    refresh_interval_secs: 5,
    stale_after_secs: 10,
    advisory_lock_key: 731_001,
}];

#[derive(Debug, FromQueryResult)]
struct VersionRow {
    version: i32,
}

#[derive(Debug, FromQueryResult)]
struct AgeRow {
    age_seconds: Option<i64>,
}

pub async fn initialize_materialized_views(
    indexer_db: DatabaseConnection,
    da_db: DatabaseConnection,
) -> Result<()> {
    if indexer_db.get_database_backend() == DatabaseBackend::Postgres {
        info!("Initializing indexer materialized views for metrics API");
        setup_and_start_for_db(indexer_db, INDEXER_VIEW_SPECS, "indexer").await?;
    }

    if da_db.get_database_backend() == DatabaseBackend::Postgres {
        info!("Initializing DA materialized views for metrics API");
        setup_and_start_for_db(da_db, DA_VIEW_SPECS, "da").await?;
    }

    Ok(())
}

async fn setup_and_start_for_db(
    db: DatabaseConnection,
    specs: &'static [ViewSpec],
    db_label: &'static str,
) -> Result<()> {
    ensure_version_table(&db)
        .await
        .with_context(|| format!("Failed to ensure MV version table on {db_label} DB"))?;

    for spec in specs {
        ensure_view_schema(&db, *spec, db_label).await?;
        if is_view_stale(&db, *spec).await? {
            refresh_materialized_view(&db, *spec, db_label)
                .await
                .with_context(|| {
                    format!(
                        "Failed to refresh stale materialized view {} on {db_label} DB",
                        spec.view_name
                    )
                })?;
        }
        spawn_refresh_task(db.clone(), *spec, db_label);
    }

    Ok(())
}

async fn ensure_version_table(db: &DatabaseConnection) -> Result<()> {
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "
            CREATE TABLE IF NOT EXISTS {MATERIALIZED_VIEW_VERSION_TABLE} (
                view_name TEXT PRIMARY KEY,
                version INTEGER NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "
        ),
    );

    db.execute(stmt)
        .await
        .context("Failed to create materialized-view version table")?;

    Ok(())
}

async fn ensure_view_schema(db: &DatabaseConnection, spec: ViewSpec, db_label: &str) -> Result<()> {
    let current_version = load_view_version(db, spec.view_name)
        .await
        .with_context(|| format!("Failed to load MV schema version for {}", spec.view_name))?;

    let needs_rebuild = current_version != Some(spec.version);
    if needs_rebuild {
        let drop_stmt = Statement::from_string(
            DatabaseBackend::Postgres,
            format!("DROP MATERIALIZED VIEW IF EXISTS {}", spec.view_name),
        );

        db.execute(drop_stmt)
            .await
            .with_context(|| format!("Failed to drop outdated MV {}", spec.view_name))?;

        info!(
            view = spec.view_name,
            expected_version = spec.version,
            current_version = current_version.unwrap_or_default(),
            db = db_label,
            "Rebuilding materialized view"
        );
    }

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        spec.create_sql.to_string(),
    ))
    .await
    .with_context(|| format!("Failed to create materialized view {}", spec.view_name))?;

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        spec.create_unique_index_sql.to_string(),
    ))
    .await
    .with_context(|| format!("Failed to create unique index for {}", spec.view_name))?;

    if needs_rebuild {
        upsert_view_version(db, spec.view_name, spec.version)
            .await
            .with_context(|| format!("Failed to persist schema version for {}", spec.view_name))?;
    }

    Ok(())
}

async fn load_view_version(db: &DatabaseConnection, view_name: &str) -> Result<Option<i32>> {
    let stmt = Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        format!("SELECT version FROM {MATERIALIZED_VIEW_VERSION_TABLE} WHERE view_name = $1"),
        [view_name.into()],
    );

    let row = VersionRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed querying MV schema version")?;

    Ok(row.map(|row| row.version))
}

async fn upsert_view_version(db: &DatabaseConnection, view_name: &str, version: i32) -> Result<()> {
    let stmt = Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        format!(
            "
            INSERT INTO {MATERIALIZED_VIEW_VERSION_TABLE} (view_name, version, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (view_name)
            DO UPDATE SET
                version = EXCLUDED.version,
                updated_at = NOW()
            "
        ),
        [view_name.into(), version.into()],
    );

    db.execute(stmt)
        .await
        .context("Failed to upsert MV schema version")?;

    Ok(())
}

async fn is_view_stale(db: &DatabaseConnection, spec: ViewSpec) -> Result<bool> {
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "
            SELECT EXTRACT(EPOCH FROM (NOW() - refreshed_at))::bigint AS age_seconds
            FROM {}
            WHERE id = 1
            ",
            spec.view_name
        ),
    );

    let row = AgeRow::find_by_statement(stmt).one(db).await;
    let row = match row {
        Ok(row) => row,
        Err(error) => {
            warn!(
                view = spec.view_name,
                error = %error,
                "Failed to read MV freshness; forcing refresh"
            );
            return Ok(true);
        }
    };

    let age_seconds = row.and_then(|row| row.age_seconds).unwrap_or(i64::MAX);
    Ok(age_seconds >= spec.stale_after_secs)
}

fn spawn_refresh_task(db: DatabaseConnection, spec: ViewSpec, db_label: &'static str) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(spec.refresh_interval_secs));
        ticker.tick().await;

        loop {
            ticker.tick().await;

            if let Err(error) = refresh_materialized_view(&db, spec, db_label).await {
                warn!(
                    view = spec.view_name,
                    db = db_label,
                    error = %error,
                    "Failed periodic materialized view refresh"
                );
            }
        }
    });
}

async fn refresh_materialized_view(
    db: &DatabaseConnection,
    spec: ViewSpec,
    db_label: &str,
) -> Result<()> {
    let mut refresh_conn = db
        .get_postgres_connection_pool()
        .acquire()
        .await
        .with_context(|| {
            format!(
                "Failed to acquire Postgres connection for MV refresh {}",
                spec.view_name
            )
        })?;

    let locked = try_advisory_lock(&mut refresh_conn, spec.advisory_lock_key).await?;
    if !locked {
        debug!(
            view = spec.view_name,
            db = db_label,
            "Skipping MV refresh because advisory lock is held by another instance"
        );
        return Ok(());
    }

    let refresh_result = try_refresh_materialized_view(&mut refresh_conn, spec).await;

    if let Err(error) = release_advisory_lock(&mut refresh_conn, spec.advisory_lock_key).await {
        warn!(
            view = spec.view_name,
            db = db_label,
            error = %error,
            "Failed to release MV advisory lock"
        );
    }

    refresh_result?;

    debug!(
        view = spec.view_name,
        db = db_label,
        "Refreshed materialized view"
    );
    Ok(())
}

async fn try_refresh_materialized_view(
    refresh_conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    spec: ViewSpec,
) -> Result<()> {
    let concurrent_sql = format!("REFRESH MATERIALIZED VIEW CONCURRENTLY {}", spec.view_name);

    match sqlx::query(&concurrent_sql)
        .execute(&mut **refresh_conn)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            warn!(
                view = spec.view_name,
                error = %error,
                "Concurrent MV refresh failed; retrying without CONCURRENTLY"
            );

            let fallback_sql = format!("REFRESH MATERIALIZED VIEW {}", spec.view_name);

            sqlx::query(&fallback_sql)
                .execute(&mut **refresh_conn)
                .await
                .with_context(|| {
                    format!("Failed to refresh materialized view {}", spec.view_name)
                })?;

            Ok(())
        }
    }
}

async fn try_advisory_lock(
    refresh_conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    lock_key: i64,
) -> Result<bool> {
    let row = sqlx::query("SELECT pg_try_advisory_lock($1) AS locked")
        .bind(lock_key)
        .fetch_one(&mut **refresh_conn)
        .await
        .context("Failed to acquire MV advisory lock")?;

    row.try_get::<bool, _>("locked")
        .context("Missing 'locked' in MV advisory lock result")
}

async fn release_advisory_lock(
    refresh_conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    lock_key: i64,
) -> Result<()> {
    let row = sqlx::query("SELECT pg_advisory_unlock($1) AS locked")
        .bind(lock_key)
        .fetch_one(&mut **refresh_conn)
        .await
        .context("Failed to release MV advisory lock")?;

    let unlocked = row
        .try_get::<bool, _>("locked")
        .context("Missing 'locked' in MV advisory unlock result")?;
    if !unlocked {
        warn!(
            lock_key,
            "MV advisory unlock returned false; lock may not have been held"
        );
    }

    Ok(())
}
