use anyhow::{anyhow, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};
use std::str::FromStr;

#[derive(Clone)]
pub struct FvkStore {
    pool: SqlitePool,
}

impl FvkStore {
    pub async fn new(db_url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(db_url)?
            .create_if_missing(true)
            .disable_statement_logging();

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .context("connect sqlite")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS service_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                next_index INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .context("create service_meta")?;

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO service_meta (id, next_index)
            VALUES (1, 0)
            "#,
        )
        .execute(&pool)
        .await
        .context("init service_meta")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS issued_fvks (
                fvk_commitment BLOB PRIMARY KEY,
                fvk BLOB NOT NULL,
                seed BLOB NOT NULL,
                issued_at_ms INTEGER NOT NULL,
                index_value INTEGER,
                signature BLOB NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .context("create issued_fvks")?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS issued_fvks_index_value
            ON issued_fvks(index_value)
            "#,
        )
        .execute(&pool)
        .await
        .context("create issued_fvks_index_value")?;

        Ok(Self { pool })
    }

    pub async fn count_issued(&self) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issued_fvks")
            .fetch_one(&self.pool)
            .await
            .context("count issued_fvks")?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub async fn get_next_index(&self) -> Result<u64> {
        let v: i64 = sqlx::query_scalar("SELECT next_index FROM service_meta WHERE id = 1")
            .fetch_one(&self.pool)
            .await
            .context("get next_index")?;
        u64::try_from(v).map_err(|_| anyhow!("next_index is negative"))
    }

    /// Ensure `next_index >= min_next_index`.
    pub async fn ensure_next_index_at_least(&self, min_next_index: u64) -> Result<u64> {
        let min: i64 = min_next_index
            .try_into()
            .map_err(|_| anyhow!("min_next_index too large"))?;

        sqlx::query(
            r#"
            UPDATE service_meta
            SET next_index = CASE WHEN next_index < ?1 THEN ?1 ELSE next_index END
            WHERE id = 1
            "#,
        )
        .bind(min)
        .execute(&self.pool)
        .await
        .context("update next_index")?;

        self.get_next_index().await
    }

    /// Allocate the next monotonically increasing index and persist it.
    pub async fn allocate_index(&self) -> Result<u64> {
        // Prefer an atomic increment using SQLite's `RETURNING` clause.
        // If `RETURNING` isn't supported by the runtime SQLite version, fall back to a transaction.
        let returning: Result<i64, sqlx::Error> = sqlx::query_scalar(
            r#"
            UPDATE service_meta
            SET next_index = next_index + 1
            WHERE id = 1
            RETURNING next_index - 1
            "#,
        )
        .fetch_one(&self.pool)
        .await;

        match returning {
            Ok(v) => u64::try_from(v).map_err(|_| anyhow!("allocated index is negative")),
            Err(e) => {
                if sqlite_returning_unsupported(&e) {
                    self.allocate_index_fallback().await
                } else {
                    Err(anyhow!(e)).context("allocate index")
                }
            }
        }
    }

    async fn allocate_index_fallback(&self) -> Result<u64> {
        let mut tx = self.pool.begin().await.context("begin tx")?;

        let current: i64 = sqlx::query_scalar("SELECT next_index FROM service_meta WHERE id = 1")
            .fetch_one(&mut *tx)
            .await
            .context("select next_index")?;

        let next = current
            .checked_add(1)
            .ok_or_else(|| anyhow!("next_index overflow"))?;

        sqlx::query("UPDATE service_meta SET next_index = ?1 WHERE id = 1")
            .bind(next)
            .execute(&mut *tx)
            .await
            .context("update next_index")?;

        tx.commit().await.context("commit tx")?;
        u64::try_from(current).map_err(|_| anyhow!("allocated index is negative"))
    }

    pub async fn record_issue(
        &self,
        fvk_commitment: &[u8; 32],
        fvk: &[u8; 32],
        seed: &[u8],
        issued_at_ms: i64,
        index_value: Option<u64>,
        signature: &[u8; 64],
    ) -> Result<()> {
        let index_value: Option<i64> = match index_value {
            Some(v) => Some(v.try_into().map_err(|_| anyhow!("index_value too large"))?),
            None => None,
        };

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO issued_fvks (
                fvk_commitment, fvk, seed, issued_at_ms, index_value, signature
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
        )
        .bind(fvk_commitment.as_slice())
        .bind(fvk.as_slice())
        .bind(seed)
        .bind(issued_at_ms)
        .bind(index_value)
        .bind(signature.as_slice())
        .execute(&self.pool)
        .await
        .context("insert issued_fvks")?;

        Ok(())
    }
}

fn sqlite_returning_unsupported(err: &sqlx::Error) -> bool {
    let msg = match err {
        sqlx::Error::Database(db) => db.message(),
        _ => return false,
    };
    msg.contains("RETURNING") || msg.contains("near \"RETURNING\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allocate_index_is_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let db = format!("sqlite://{}/fvk.sqlite?mode=rwc", dir.path().display());
        let store = FvkStore::new(&db).await.unwrap();

        assert_eq!(store.get_next_index().await.unwrap(), 0);
        assert_eq!(store.allocate_index().await.unwrap(), 0);
        assert_eq!(store.allocate_index().await.unwrap(), 1);
        assert_eq!(store.get_next_index().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn ensure_next_index_at_least_is_largest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let db = format!("sqlite://{}/fvk.sqlite?mode=rwc", dir.path().display());
        let store = FvkStore::new(&db).await.unwrap();

        assert_eq!(store.get_next_index().await.unwrap(), 0);
        assert_eq!(store.ensure_next_index_at_least(10).await.unwrap(), 10);
        assert_eq!(store.ensure_next_index_at_least(3).await.unwrap(), 10);
        assert_eq!(store.ensure_next_index_at_least(12).await.unwrap(), 12);
    }
}
