use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, FromRow, SqlitePool};
use std::str::FromStr;

/// Persistent (in-memory) transaction store for the wallet.
/// Uses SQLite with a shared cache so the pool shares one in-memory database.
#[derive(Clone)]
pub struct TransactionStore {
    pool: SqlitePool,
}

#[derive(Debug, Clone, FromRow)]
pub struct StoredTransaction {
    pub id: String,
    pub state: String,
    pub from_address: Option<String>,
    pub to_address: Option<String>,
    pub amount: Option<String>,
    pub tx_identifier: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub error_message: Option<String>,
}

/// Upsert payload for a transaction record.
#[derive(Debug, Clone)]
pub struct TransactionUpsert {
    pub id: String,
    pub state: String,
    pub from_address: Option<String>,
    pub to_address: Option<String>,
    pub amount: Option<String>,
    pub tx_identifier: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub error_message: Option<String>,
}

/// Sync summary from the local store's perspective.
#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub total: u64,
    pub pending: u64,
    pub is_synced: bool,
}

impl TransactionStore {
    pub async fn new_in_memory() -> Result<Self> {
        const CREATE_SQL: &str = r#"
            CREATE TABLE IF NOT EXISTS transactions (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                from_address TEXT,
                to_address TEXT,
                amount TEXT,
                tx_identifier TEXT UNIQUE,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                error_message TEXT
            );
        "#;

        let options = SqliteConnectOptions::from_str("sqlite::memory:?cache=shared")?
            .create_if_missing(true)
            .disable_statement_logging();

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query(CREATE_SQL).execute(conn).await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;

        sqlx::query(CREATE_SQL).execute(&pool).await?;

        Ok(Self { pool })
    }

    pub async fn upsert(&self, upsert: TransactionUpsert) -> Result<()> {
        // If we already have this tx_identifier, prefer to update in place to avoid UNIQUE conflicts.
        if let Some(ref txid) = upsert.tx_identifier {
            let res = sqlx::query(
                r#"
                UPDATE transactions
                SET state=?2,
                    from_address=COALESCE(?3, from_address),
                    to_address=COALESCE(?4, to_address),
                    amount=COALESCE(?5, amount),
                    tx_identifier=COALESCE(?6, tx_identifier),
                    updated_at=?7,
                    error_message=?8
                WHERE tx_identifier=?1
                "#,
            )
            .bind(txid)
            .bind(&upsert.state)
            .bind(&upsert.from_address)
            .bind(&upsert.to_address)
            .bind(&upsert.amount)
            .bind(&upsert.tx_identifier)
            .bind(upsert.updated_at)
            .bind(&upsert.error_message)
            .execute(&self.pool)
            .await?;

            if res.rows_affected() > 0 {
                return Ok(());
            }
        }

        sqlx::query(
            r#"
            INSERT INTO transactions (
                id, state, from_address, to_address, amount,
                tx_identifier, created_at, updated_at, error_message
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(id) DO UPDATE SET
                state=excluded.state,
                from_address=COALESCE(excluded.from_address, transactions.from_address),
                to_address=COALESCE(excluded.to_address, transactions.to_address),
                amount=COALESCE(excluded.amount, transactions.amount),
                tx_identifier=COALESCE(excluded.tx_identifier, transactions.tx_identifier),
                created_at=MIN(transactions.created_at, excluded.created_at),
                updated_at=excluded.updated_at,
                error_message=excluded.error_message
            "#,
        )
        .bind(upsert.id)
        .bind(upsert.state)
        .bind(upsert.from_address)
        .bind(upsert.to_address)
        .bind(upsert.amount)
        .bind(upsert.tx_identifier)
        .bind(upsert.created_at)
        .bind(upsert.updated_at)
        .bind(upsert.error_message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_by_id(&self, id: &str) -> Result<Option<StoredTransaction>> {
        let row = sqlx::query_as::<_, StoredTransaction>(
            r#"
            SELECT id, state, from_address, to_address, amount,
                   tx_identifier, created_at, updated_at, error_message
            FROM transactions
            WHERE id = ?1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    #[allow(dead_code)]
    pub async fn list_all(&self) -> Result<Vec<StoredTransaction>> {
        let rows = sqlx::query_as::<_, StoredTransaction>(
            r#"
            SELECT id, state, from_address, to_address, amount,
                   tx_identifier, created_at, updated_at, error_message
            FROM transactions
            ORDER BY updated_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    pub async fn mark_failed(&self, id: &str, error: &str, now: i64) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE transactions
            SET state='failed', error_message=?2, updated_at=?3
            WHERE id=?1
            "#,
        )
        .bind(id)
        .bind(error)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_state(
        &self,
        id: &str,
        state: &str,
        tx_identifier: Option<&str>,
        error_message: Option<&str>,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE transactions
            SET state=?2,
                tx_identifier=COALESCE(?3, tx_identifier),
                error_message=?4,
                updated_at=?5
            WHERE id=?1
            "#,
        )
        .bind(id)
        .bind(state)
        .bind(tx_identifier)
        .bind(error_message)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn count_all(&self) -> Result<u64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) as count FROM transactions")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 as u64)
    }

    pub async fn count_pending(&self) -> Result<u64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) as count FROM transactions WHERE state IN ('initiated','sent')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 as u64)
    }

    pub async fn summary(&self) -> Result<SyncSummary> {
        let total = self.count_all().await?;
        let pending = self.count_pending().await?;
        Ok(SyncSummary {
            total,
            pending,
            is_synced: pending == 0,
        })
    }
}
