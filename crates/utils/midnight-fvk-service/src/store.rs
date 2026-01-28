use anyhow::{anyhow, Context, Result};
use sqlx::any::{AnyConnectOptions, AnyPoolOptions};
use sqlx::{AnyPool, Row};
use std::str::FromStr;

#[derive(Clone)]
pub struct FvkStore {
    pool: AnyPool,
    is_postgres: bool,
}

impl FvkStore {
    pub async fn new(db_url: &str) -> Result<Self> {
        // Install the any driver (required for AnyPool)
        sqlx::any::install_default_drivers();

        let is_postgres = db_url.starts_with("postgres://") || db_url.starts_with("postgresql://");

        let options = AnyConnectOptions::from_str(db_url).context("parse database URL")?;

        let pool = AnyPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .context("connect to database")?;

        let store = Self { pool, is_postgres };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> Result<()> {
        if self.is_postgres {
            self.init_schema_postgres().await
        } else {
            self.init_schema_sqlite().await
        }
    }

    async fn init_schema_sqlite(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS service_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                next_index INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create service_meta")?;

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO service_meta (id, next_index)
            VALUES (1, 0)
            "#,
        )
        .execute(&self.pool)
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
                signature BLOB NOT NULL,
                shielded_address TEXT,
                wallet_address TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create issued_fvks")?;

        // Migration: add shielded_address column if it doesn't exist (for existing DBs)
        let _ = sqlx::query("ALTER TABLE issued_fvks ADD COLUMN shielded_address TEXT")
            .execute(&self.pool)
            .await;

        // Migration: add wallet_address column if it doesn't exist (for existing DBs)
        let _ = sqlx::query("ALTER TABLE issued_fvks ADD COLUMN wallet_address TEXT")
            .execute(&self.pool)
            .await;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS issued_fvks_index_value
            ON issued_fvks(index_value)
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create issued_fvks_index_value")?;

        Ok(())
    }

    async fn init_schema_postgres(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS service_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                next_index BIGINT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create service_meta")?;

        sqlx::query(
            r#"
            INSERT INTO service_meta (id, next_index)
            VALUES (1, 0)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .execute(&self.pool)
        .await
        .context("init service_meta")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS issued_fvks (
                fvk_commitment BYTEA PRIMARY KEY,
                fvk BYTEA NOT NULL,
                seed BYTEA NOT NULL,
                issued_at_ms BIGINT NOT NULL,
                index_value BIGINT,
                signature BYTEA NOT NULL,
                shielded_address TEXT,
                wallet_address TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create issued_fvks")?;

        // Migration: add shielded_address column if it doesn't exist (for existing DBs)
        let _ =
            sqlx::query("ALTER TABLE issued_fvks ADD COLUMN IF NOT EXISTS shielded_address TEXT")
                .execute(&self.pool)
                .await;

        // Migration: add wallet_address column if it doesn't exist (for existing DBs)
        let _ = sqlx::query("ALTER TABLE issued_fvks ADD COLUMN IF NOT EXISTS wallet_address TEXT")
            .execute(&self.pool)
            .await;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS issued_fvks_index_value
            ON issued_fvks(index_value)
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create issued_fvks_index_value")?;

        Ok(())
    }

    pub async fn count_issued(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM issued_fvks")
            .fetch_one(&self.pool)
            .await
            .context("count issued_fvks")?;
        let count: i64 = row.try_get("cnt")?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub async fn get_next_index(&self) -> Result<u64> {
        let row = sqlx::query("SELECT next_index FROM service_meta WHERE id = 1")
            .fetch_one(&self.pool)
            .await
            .context("get next_index")?;
        let v: i64 = row.try_get("next_index")?;
        u64::try_from(v).map_err(|_| anyhow!("next_index is negative"))
    }

    /// Ensure `next_index >= min_next_index`.
    pub async fn ensure_next_index_at_least(&self, min_next_index: u64) -> Result<u64> {
        let min: i64 = min_next_index
            .try_into()
            .map_err(|_| anyhow!("min_next_index too large"))?;

        if self.is_postgres {
            sqlx::query(
                r#"
                UPDATE service_meta
                SET next_index = CASE WHEN next_index < $1 THEN $1 ELSE next_index END
                WHERE id = 1
                "#,
            )
            .bind(min)
            .execute(&self.pool)
            .await
            .context("update next_index")?;
        } else {
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
        }

        self.get_next_index().await
    }

    /// Allocate the next monotonically increasing index and persist it.
    pub async fn allocate_index(&self) -> Result<u64> {
        if self.is_postgres {
            self.allocate_index_postgres().await
        } else {
            self.allocate_index_sqlite().await
        }
    }

    async fn allocate_index_postgres(&self) -> Result<u64> {
        let row = sqlx::query(
            r#"
            UPDATE service_meta
            SET next_index = next_index + 1
            WHERE id = 1
            RETURNING next_index - 1 as allocated
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .context("allocate index")?;

        let v: i64 = row.try_get("allocated")?;
        u64::try_from(v).map_err(|_| anyhow!("allocated index is negative"))
    }

    async fn allocate_index_sqlite(&self) -> Result<u64> {
        // Prefer an atomic increment using SQLite's `RETURNING` clause.
        // If `RETURNING` isn't supported by the runtime SQLite version, fall back to a transaction.
        let returning: Result<_, sqlx::Error> = sqlx::query(
            r#"
            UPDATE service_meta
            SET next_index = next_index + 1
            WHERE id = 1
            RETURNING next_index - 1 as allocated
            "#,
        )
        .fetch_one(&self.pool)
        .await;

        match returning {
            Ok(row) => {
                let v: i64 = row.try_get("allocated")?;
                u64::try_from(v).map_err(|_| anyhow!("allocated index is negative"))
            }
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

        let row = sqlx::query("SELECT next_index FROM service_meta WHERE id = 1")
            .fetch_one(&mut *tx)
            .await
            .context("select next_index")?;
        let current: i64 = row.try_get("next_index")?;

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
        shielded_address: Option<&str>,
        wallet_address: Option<&str>,
    ) -> Result<()> {
        let index_value: Option<i64> = match index_value {
            Some(v) => Some(v.try_into().map_err(|_| anyhow!("index_value too large"))?),
            None => None,
        };

        if self.is_postgres {
            sqlx::query(
                r#"
                INSERT INTO issued_fvks (
                    fvk_commitment, fvk, seed, issued_at_ms, index_value, signature, shielded_address, wallet_address
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (fvk_commitment) DO NOTHING
                "#,
            )
            .bind(fvk_commitment.as_slice())
            .bind(fvk.as_slice())
            .bind(seed)
            .bind(issued_at_ms)
            .bind(index_value)
            .bind(signature.as_slice())
            .bind(shielded_address)
            .bind(wallet_address)
            .execute(&self.pool)
            .await
            .context("insert issued_fvks")?;
        } else {
            sqlx::query(
                r#"
                INSERT OR IGNORE INTO issued_fvks (
                    fvk_commitment, fvk, seed, issued_at_ms, index_value, signature, shielded_address, wallet_address
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )
            .bind(fvk_commitment.as_slice())
            .bind(fvk.as_slice())
            .bind(seed)
            .bind(issued_at_ms)
            .bind(index_value)
            .bind(signature.as_slice())
            .bind(shielded_address)
            .bind(wallet_address)
            .execute(&self.pool)
            .await
            .context("insert issued_fvks")?;
        }

        Ok(())
    }

    pub async fn get_fvk_by_commitment(
        &self,
        fvk_commitment: &[u8; 32],
    ) -> Result<Option<([u8; 32], Option<String>, Option<String>)>> {
        let row = if self.is_postgres {
            sqlx::query(
                r#"
                SELECT fvk, shielded_address, wallet_address
                FROM issued_fvks
                WHERE fvk_commitment = $1
                "#,
            )
            .bind(fvk_commitment.as_slice())
            .fetch_optional(&self.pool)
            .await
            .context("select fvk by commitment")?
        } else {
            sqlx::query(
                r#"
                SELECT fvk, shielded_address, wallet_address
                FROM issued_fvks
                WHERE fvk_commitment = ?1
                "#,
            )
            .bind(fvk_commitment.as_slice())
            .fetch_optional(&self.pool)
            .await
            .context("select fvk by commitment")?
        };

        let Some(row) = row else {
            return Ok(None);
        };

        let bytes: Vec<u8> = row.try_get("fvk")?;
        let shielded_address: Option<String> = row.try_get("shielded_address")?;
        let wallet_address: Option<String> = row.try_get("wallet_address")?;

        let len = bytes.len();
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow!("fvk must be 32 bytes (got {len})"))?;
        Ok(Some((bytes, shielded_address, wallet_address)))
    }

    /// Update the addresses for an existing FVK entry
    pub async fn update_addresses(
        &self,
        fvk_commitment: &[u8; 32],
        shielded_address: Option<&str>,
        wallet_address: Option<&str>,
    ) -> Result<bool> {
        let result = if self.is_postgres {
            sqlx::query(
                r#"
                UPDATE issued_fvks
                SET shielded_address = COALESCE($2, shielded_address),
                    wallet_address = COALESCE($3, wallet_address)
                WHERE fvk_commitment = $1
                "#,
            )
            .bind(fvk_commitment.as_slice())
            .bind(shielded_address)
            .bind(wallet_address)
            .execute(&self.pool)
            .await
            .context("update addresses")?
        } else {
            sqlx::query(
                r#"
                UPDATE issued_fvks
                SET shielded_address = COALESCE(?2, shielded_address),
                    wallet_address = COALESCE(?3, wallet_address)
                WHERE fvk_commitment = ?1
                "#,
            )
            .bind(fvk_commitment.as_slice())
            .bind(shielded_address)
            .bind(wallet_address)
            .execute(&self.pool)
            .await
            .context("update addresses")?
        };

        Ok(result.rows_affected() > 0)
    }

    /// List all issued FVKs with their addresses
    pub async fn list_all(
        &self,
    ) -> Result<Vec<([u8; 32], [u8; 32], Option<String>, Option<String>)>> {
        let rows = sqlx::query(
            r#"
            SELECT fvk_commitment, fvk, shielded_address, wallet_address
            FROM issued_fvks
            ORDER BY issued_at_ms DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("list all fvks")?;

        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let commitment_bytes: Vec<u8> = row.try_get("fvk_commitment")?;
            let fvk_bytes: Vec<u8> = row.try_get("fvk")?;
            let shielded_address: Option<String> = row.try_get("shielded_address")?;
            let wallet_address: Option<String> = row.try_get("wallet_address")?;

            let commitment: [u8; 32] = commitment_bytes
                .try_into()
                .map_err(|_| anyhow!("fvk_commitment must be 32 bytes"))?;
            let fvk: [u8; 32] = fvk_bytes
                .try_into()
                .map_err(|_| anyhow!("fvk must be 32 bytes"))?;
            result.push((commitment, fvk, shielded_address, wallet_address));
        }
        Ok(result)
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
