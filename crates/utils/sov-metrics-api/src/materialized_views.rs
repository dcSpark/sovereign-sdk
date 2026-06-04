use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sea_orm::sqlx::{self, Row};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

pub const INDEXER_TRANSFER_TOTALS_VIEW: &str = "mv_metrics_transfer_totals";
pub const INDEXER_DEPOSIT_TOTALS_VIEW: &str = "mv_metrics_deposit_totals";
pub const INDEXER_TRANSFER_STATS_24H_VIEW: &str = "mv_metrics_transfer_stats_24h";
pub const INDEXER_ACCOUNT_TOTALS_VIEW: &str = "mv_metrics_account_totals";
pub const DA_WORKER_TX_TOTALS_VIEW: &str = "mv_metrics_worker_tx_totals";

const MATERIALIZED_VIEW_VERSION_TABLE: &str = "metrics_materialized_view_versions";

#[derive(Clone, Copy, Debug)]
pub struct RefreshPolicy {
    pub enabled: bool,
    pub interval_multiplier: u64,
    pub min_interval_secs: u64,
}

impl RefreshPolicy {
    fn effective_refresh_interval_secs(self, spec: ViewSpec) -> u64 {
        spec.refresh_interval_secs
            .saturating_mul(self.interval_multiplier.max(1))
            .max(self.min_interval_secs)
    }

    fn effective_stale_after_secs(self, spec: ViewSpec) -> i64 {
        let interval_secs = self.effective_refresh_interval_secs(spec);
        let interval_secs = if interval_secs > i64::MAX as u64 {
            i64::MAX
        } else {
            interval_secs as i64
        };

        spec.stale_after_secs.max(interval_secs)
    }
}

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
        version: 2,
        create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_transfer_totals AS
WITH stats AS (
    SELECT
        pg_class.reltuples::numeric AS estimated_rows
    FROM pg_class
    WHERE pg_class.oid = 'midnight_transfer'::regclass::oid
),
sample AS (
    SELECT
        count(*)::numeric AS sample_rows,
        count(*) FILTER (WHERE midnight_transfer.amount::text ~ '^[0-9]+$'::text)::numeric AS valid_rows,
        avg(
            CASE
                WHEN midnight_transfer.amount::text ~ '^[0-9]+$'::text
                    THEN midnight_transfer.amount::numeric
                ELSE NULL::numeric
            END
        ) AS avg_valid_amount
    FROM midnight_transfer TABLESAMPLE system (0.1)
)
SELECT
    1 AS id,
    COALESCE(
        round(
            (
                (SELECT stats.estimated_rows FROM stats)
                * COALESCE(
                    (SELECT sample.valid_rows / NULLIF(sample.sample_rows, 0::numeric) FROM sample),
                    0::numeric
                )
                * COALESCE((SELECT sample.avg_valid_amount FROM sample), 0::numeric)
            ),
            0
        )::text,
        '0'::text
    ) AS total_amount,
    COALESCE(
        round(
            (
                (SELECT stats.estimated_rows FROM stats)
                * COALESCE(
                    (SELECT sample.valid_rows / NULLIF(sample.sample_rows, 0::numeric) FROM sample),
                    0::numeric
                )
            ),
            0
        )::bigint,
        0::bigint
    ) AS total_transactions,
    now() AS refreshed_at
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
        version: 2,
        create_sql: r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_metrics_account_totals AS
WITH recent_events AS (
    SELECT e.id
    FROM events e
    WHERE e.created_at >= (now() - '00:05:00'::interval)
),
recent_senders AS (
    SELECT t.privacy_sender
    FROM recent_events re
    JOIN midnight_transfer t ON t.event_id = re.id
    WHERE t.privacy_sender IS NOT NULL
    UNION
    SELECT w.privacy_sender
    FROM recent_events re
    JOIN midnight_withdraw w ON w.event_id = re.id
    WHERE w.privacy_sender IS NOT NULL
)
SELECT
    1 AS id,
    (SELECT count(*) AS count FROM fvk_registry) AS total_accounts,
    COALESCE(
        (
            SELECT round(c.reltuples * (1::double precision - s.null_frac))::bigint AS round
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_stats s ON s.schemaname = n.nspname AND s.tablename = c.relname
            WHERE n.nspname = 'public'::name
              AND c.relname = 'midnight_transfer'::name
              AND s.attname = 'view_attestations'::name
        ),
        0::bigint
    ) + (
        SELECT count(*) AS count
        FROM midnight_withdraw
        WHERE midnight_withdraw.view_attestations IS NOT NULL
    ) AS total_disclosure_events,
    (SELECT count(*) AS count FROM recent_senders) AS sending_accounts_5m,
    now() AS refreshed_at
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
    refresh_policy: RefreshPolicy,
) -> Result<()> {
    if indexer_db.get_database_backend() == DatabaseBackend::Postgres {
        info!("Initializing indexer materialized views for metrics API");
        setup_and_start_for_db(indexer_db, INDEXER_VIEW_SPECS, "indexer", refresh_policy).await?;
    }

    if da_db.get_database_backend() == DatabaseBackend::Postgres {
        info!("Initializing DA materialized views for metrics API");
        setup_and_start_for_db(da_db, DA_VIEW_SPECS, "da", refresh_policy).await?;
    }

    Ok(())
}

async fn setup_and_start_for_db(
    db: DatabaseConnection,
    specs: &'static [ViewSpec],
    db_label: &'static str,
    refresh_policy: RefreshPolicy,
) -> Result<()> {
    ensure_version_table(&db)
        .await
        .with_context(|| format!("Failed to ensure MV version table on {db_label} DB"))?;

    for spec in specs {
        ensure_view_schema(&db, *spec, db_label).await?;

        if !refresh_policy.enabled {
            info!(
                view = spec.view_name,
                db = db_label,
                "Skipping materialized view auto-refresh because it is disabled"
            );
            continue;
        }

        let stale_after_secs = refresh_policy.effective_stale_after_secs(*spec);
        if is_view_stale(&db, *spec, stale_after_secs).await? {
            refresh_materialized_view(&db, *spec, db_label, true)
                .await
                .with_context(|| {
                    format!(
                        "Failed to refresh stale materialized view {} on {db_label} DB",
                        spec.view_name
                    )
                })?;
        }
        spawn_refresh_task(db.clone(), *spec, db_label, refresh_policy);
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

async fn is_view_stale(
    db: &DatabaseConnection,
    spec: ViewSpec,
    stale_after_secs: i64,
) -> Result<bool> {
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
    Ok(age_seconds >= stale_after_secs)
}

fn spawn_refresh_task(
    db: DatabaseConnection,
    spec: ViewSpec,
    db_label: &'static str,
    refresh_policy: RefreshPolicy,
) {
    tokio::spawn(async move {
        let refresh_interval_secs = refresh_policy.effective_refresh_interval_secs(spec);
        let stale_after_secs = refresh_policy.effective_stale_after_secs(spec);

        info!(
            view = spec.view_name,
            db = db_label,
            refresh_interval_secs,
            stale_after_secs,
            "Starting periodic materialized view refresh"
        );

        let mut ticker = interval(Duration::from_secs(refresh_interval_secs));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            ticker.tick().await;

            match is_view_stale(&db, spec, stale_after_secs).await {
                Ok(false) => continue,
                Ok(true) => {}
                Err(error) => {
                    warn!(
                        view = spec.view_name,
                        db = db_label,
                        error = %error,
                        "Failed to check MV staleness; forcing refresh"
                    );
                }
            }

            if let Err(error) = refresh_materialized_view(&db, spec, db_label, false).await {
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
    allow_blocking_fallback: bool,
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

    let refresh_result =
        try_refresh_materialized_view(&mut refresh_conn, spec, allow_blocking_fallback).await;

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
    allow_blocking_fallback: bool,
) -> Result<()> {
    let concurrent_sql = format!("REFRESH MATERIALIZED VIEW CONCURRENTLY {}", spec.view_name);

    match sqlx::query(&concurrent_sql)
        .execute(&mut **refresh_conn)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            let fallback_allowed =
                allow_blocking_fallback && should_use_blocking_refresh_fallback(&error);

            warn!(
                view = spec.view_name,
                error = %error,
                allow_blocking_fallback,
                fallback_allowed,
                "Concurrent MV refresh failed"
            );

            if !fallback_allowed {
                return Err(anyhow!(error).context(format!(
                    "Failed to refresh materialized view {}",
                    spec.view_name
                )));
            }

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

fn should_use_blocking_refresh_fallback(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db_error) => {
            // Restrict blocking fallback to PostgreSQL prerequisite/unsupported states.
            // This avoids turning transient concurrent-refresh failures into heavier locks.
            matches!(db_error.code().as_deref(), Some("55000") | Some("0A000"))
        }
        _ => false,
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
    // Avoid calling pg_advisory_unlock when the session no longer owns this lock key.
    // Calling unlock without ownership emits a PostgreSQL notice:
    // "you don't own a lock of type ExclusiveLock".
    let row = sqlx::query(
        r#"
WITH key_parts AS (
    SELECT
        (($1::bigint >> 32) & 4294967295)::oid AS classid,
        ($1::bigint & 4294967295)::oid AS objid
)
SELECT CASE
    WHEN EXISTS (
        SELECT 1
        FROM pg_locks l
        JOIN key_parts k ON l.classid = k.classid AND l.objid = k.objid
        WHERE l.locktype = 'advisory'
          AND l.pid = pg_backend_pid()
          AND l.objsubid = 1
    )
    THEN pg_advisory_unlock($1::bigint)
    ELSE TRUE
END AS locked
"#,
    )
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
