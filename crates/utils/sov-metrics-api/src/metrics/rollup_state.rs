use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};

const TABLE_NAME: &str = "metrics_rollup_state";

#[derive(Clone, Debug, Default, FromQueryResult)]
pub struct RollupState {
    pub last_id: i64,
    pub last_timestamp_ms: Option<i64>,
    pub total_amount: String,
    pub total_transactions: i64,
    pub failed_transactions: i64,
}

impl RollupState {
    pub fn empty() -> Self {
        Self {
            total_amount: "0".to_string(),
            ..Default::default()
        }
    }
}

pub async fn ensure_table(db: &DatabaseConnection) -> Result<()> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            format!(
                r#"
                CREATE TABLE IF NOT EXISTS {TABLE_NAME} (
                    key TEXT PRIMARY KEY,
                    last_id BIGINT NOT NULL DEFAULT 0,
                    last_timestamp_ms BIGINT NULL,
                    total_amount TEXT NOT NULL DEFAULT '0',
                    total_transactions BIGINT NOT NULL DEFAULT 0,
                    failed_transactions BIGINT NOT NULL DEFAULT 0
                )
                "#
            )
        }
        DatabaseBackend::Sqlite => {
            format!(
                r#"
                CREATE TABLE IF NOT EXISTS {TABLE_NAME} (
                    key TEXT PRIMARY KEY,
                    last_id BIGINT NOT NULL DEFAULT 0,
                    last_timestamp_ms BIGINT NULL,
                    total_amount TEXT NOT NULL DEFAULT '0',
                    total_transactions BIGINT NOT NULL DEFAULT 0,
                    failed_transactions BIGINT NOT NULL DEFAULT 0
                )
                "#
            )
        }
        _ => {
            format!(
                r#"
                CREATE TABLE IF NOT EXISTS {TABLE_NAME} (
                    key TEXT PRIMARY KEY,
                    last_id BIGINT NOT NULL DEFAULT 0,
                    last_timestamp_ms BIGINT NULL,
                    total_amount TEXT NOT NULL,
                    total_transactions BIGINT NOT NULL,
                    failed_transactions BIGINT NOT NULL
                )
                "#
            )
        }
    };

    db.execute(Statement::from_string(backend, sql))
        .await
        .context("Failed to create metrics rollup state table")?;
    Ok(())
}

pub async fn load(db: &DatabaseConnection, key: &str) -> Result<Option<RollupState>> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            format!(
                r#"
                SELECT last_id, last_timestamp_ms, total_amount, total_transactions, failed_transactions
                FROM {TABLE_NAME}
                WHERE key = $1
                "#
            )
        }
        _ => {
            format!(
                r#"
                SELECT last_id, last_timestamp_ms, total_amount, total_transactions, failed_transactions
                FROM {TABLE_NAME}
                WHERE key = ?1
                "#
            )
        }
    };

    RollupState::find_by_statement(Statement::from_sql_and_values(backend, sql, [key.into()]))
        .one(db)
        .await
        .with_context(|| format!("Failed to load metrics rollup state {key}"))
}

pub async fn save(db: &DatabaseConnection, key: &str, state: &RollupState) -> Result<()> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            format!(
                r#"
                INSERT INTO {TABLE_NAME}
                    (key, last_id, last_timestamp_ms, total_amount, total_transactions, failed_transactions)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (key) DO UPDATE SET
                    last_id = EXCLUDED.last_id,
                    last_timestamp_ms = EXCLUDED.last_timestamp_ms,
                    total_amount = EXCLUDED.total_amount,
                    total_transactions = EXCLUDED.total_transactions,
                    failed_transactions = EXCLUDED.failed_transactions
                "#
            )
        }
        _ => {
            format!(
                r#"
                INSERT INTO {TABLE_NAME}
                    (key, last_id, last_timestamp_ms, total_amount, total_transactions, failed_transactions)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(key) DO UPDATE SET
                    last_id = excluded.last_id,
                    last_timestamp_ms = excluded.last_timestamp_ms,
                    total_amount = excluded.total_amount,
                    total_transactions = excluded.total_transactions,
                    failed_transactions = excluded.failed_transactions
                "#
            )
        }
    };

    db.execute(Statement::from_sql_and_values(
        backend,
        sql,
        vec![
            key.into(),
            state.last_id.into(),
            state.last_timestamp_ms.into(),
            state.total_amount.clone().into(),
            state.total_transactions.into(),
            state.failed_transactions.into(),
        ],
    ))
    .await
    .with_context(|| format!("Failed to save metrics rollup state {key}"))?;
    Ok(())
}
