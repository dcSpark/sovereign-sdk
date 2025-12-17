use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::{Mutex, OnceLock};
use std::sync::Once;
use std::time::Duration;

use anyhow::anyhow;
use sea_orm::{
    ColumnTrait, ConnectOptions, Database, DatabaseConnection, EntityTrait, FromQueryResult,
    QueryFilter, QuerySelect,
};
use sov_midnight_da::storable::worker_verified_transactions;
use sov_rollup_interface::TxHash;
use tokio::{runtime::Handle, task, sync::OnceCell};

use crate::{Hash32, SpendPublic};

type PreVerifiedMap = HashMap<Hash32, SpendPublic>;

#[derive(Clone, Debug, FromQueryResult)]
struct ProofOutputsRow {
    proof_verified: Option<bool>,
    proof_outputs: String,
}

static PRE_VERIFIED_SPENDS: OnceLock<Mutex<PreVerifiedMap>> = OnceLock::new();
static PRIMED_TX_HASHES: OnceLock<Mutex<HashSet<TxHash>>> = OnceLock::new();
static WORKER_DB: OnceCell<DatabaseConnection> = OnceCell::const_new();
static WARN_MISSING_WORKER_DB_CONN: Once = Once::new();

fn map() -> &'static Mutex<PreVerifiedMap> {
    PRE_VERIFIED_SPENDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn primed_hashes() -> &'static Mutex<HashSet<TxHash>> {
    PRIMED_TX_HASHES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Best-effort: hydrate the in-process cache from the persisted worker_verified_transactions table
/// using the transaction hash as the key. This allows restarts to recover the proof outputs
/// instead of relying on an in-memory map populated by prior requests.
pub fn prime_pre_verified_spend(tx_hash: &TxHash) {
    if env::var_os("SOV_WORKER_TX_DB_CONNECTION_STRING").is_none() {
        WARN_MISSING_WORKER_DB_CONN.call_once(|| {
            tracing::warn!(
                target: "midnight_privacy::preverified",
                "SOV_WORKER_TX_DB_CONNECTION_STRING is not set; cannot hydrate pre-verified proofs from worker_verified_transactions"
            );
        });
        return;
    }

    {
        let guard = primed_hashes().lock().unwrap();
        if guard.contains(tx_hash) {
            return;
        }
    }

    match fetch_proof_outputs(tx_hash) {
        Ok(Some(public)) => {
            let mut guard = map().lock().unwrap();
            guard.insert(public.nullifier, public.clone());
            primed_hashes().lock().unwrap().insert(*tx_hash);
            tracing::debug!(
                target: "midnight_privacy::preverified",
                "[PRE-VERIFIED] hydrated proof outputs from DB for tx_hash={tx_hash} (nullifier={:?})",
                public.nullifier
            );
        }
        Ok(None) => {
            tracing::debug!(
                target: "midnight_privacy::preverified",
                "[PRE-VERIFIED] no verified proof_outputs available for tx_hash={tx_hash}"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "midnight_privacy::preverified",
                "[PRE-VERIFIED] failed to hydrate proof outputs for tx_hash={tx_hash}: {err}"
            );
        }
    }
}

/// Caches pre-verified spend outputs keyed by their nullifier.
/// This remains for compatibility with existing call sites that already have the public outputs.
pub fn cache_pre_verified_spend(public: SpendPublic) {
    let mut guard = map().lock().unwrap();
    guard.insert(public.nullifier, public.clone());
    tracing::debug!(
        target: "midnight_privacy::preverified",
        "[PRE-VERIFIED] cached pre-verified spend for nullifier={:?}, map_len={}",
        public.nullifier,
        guard.len(),
    );
}

/// Retrieves a cached spend output for the provided nullifier, if any.
pub fn get_pre_verified_spend(nullifier: &Hash32) -> Option<SpendPublic> {
    map().lock().unwrap().get(nullifier).cloned()
}

/// Removes any cached spend output associated with the provided nullifier.
pub fn clear_pre_verified_spend(nullifier: &Hash32) {
    if let Some(lock) = PRE_VERIFIED_SPENDS.get() {
        lock.lock().unwrap().remove(nullifier);
    }
}

fn fetch_proof_outputs(tx_hash: &TxHash) -> anyhow::Result<Option<SpendPublic>> {
    let conn = get_worker_db()?;
    let tx_hash_str = tx_hash.to_string();
    let maybe_row: Result<_, sea_orm::DbErr> = block_on(async {
        worker_verified_transactions::Entity::find()
            .select_only()
            .column(worker_verified_transactions::Column::ProofVerified)
            .column(worker_verified_transactions::Column::ProofOutputs)
            .filter(worker_verified_transactions::Column::TxHash.eq(tx_hash_str.clone()))
            .into_model::<ProofOutputsRow>()
            .one(conn)
            .await
    })
    .map_err(|err| anyhow!("DB query for proof_outputs failed: {err}"))?;

    if let Some(row) = maybe_row? {
        if row.proof_verified != Some(true) {
            return Ok(None);
        }
        if row.proof_outputs.trim().is_empty() {
            return Ok(None);
        }
        let parsed: SpendPublic = serde_json::from_str(&row.proof_outputs)
            .map_err(|err| anyhow!("Failed to deserialize proof_outputs JSON: {err}"))?;
        Ok(Some(parsed))
    } else {
        Ok(None)
    }
}

fn get_worker_db() -> anyhow::Result<&'static DatabaseConnection> {
    let connection_string = env::var("SOV_WORKER_TX_DB_CONNECTION_STRING")
        .map_err(|_| anyhow!("SOV_WORKER_TX_DB_CONNECTION_STRING env var is not set"))?;

    let conn = block_on(async {
        WORKER_DB
            .get_or_try_init(|| async move {
                if connection_string.starts_with("sqlite:") {
                    use sea_orm::sqlx::sqlite::{
                        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
                    };
                    use std::str::FromStr;

                    let sqlite_opts = SqliteConnectOptions::from_str(&connection_string)
                        .map_err(|err| {
                            anyhow!("Failed to parse worker DB SQLite connection string: {err}")
                        })?
                        .journal_mode(SqliteJournalMode::Wal)
                        .synchronous(SqliteSynchronous::Normal)
                        .busy_timeout(Duration::from_millis(30_000));

                    let pool = SqlitePoolOptions::new()
                        .max_connections(5)
                        .min_connections(1)
                        .acquire_timeout(Duration::from_secs(30))
                        .idle_timeout(Some(Duration::from_secs(300)))
                        .max_lifetime(Some(Duration::from_secs(1800)))
                        .connect_with(sqlite_opts)
                        .await
                        .map_err(|err| anyhow!("Failed to connect to worker SQLite DB: {err}"))?;

                    Ok(DatabaseConnection::SqlxSqlitePoolConnection(pool.into()))
                } else {
                    let mut connect_opts = ConnectOptions::new(connection_string.clone());
                    connect_opts
                        .max_connections(40)
                        .min_connections(5)
                        .connect_timeout(Duration::from_secs(30))
                        .acquire_timeout(Duration::from_secs(30))
                        .idle_timeout(Duration::from_secs(300))
                        .max_lifetime(Duration::from_secs(1800))
                        .sqlx_logging(false);

                    Database::connect(connect_opts)
                        .await
                        .map_err(|err| anyhow!("Failed to connect to worker DB: {err}"))
                }
            })
            .await
    })??;

    Ok(conn)
}

fn block_on<F, T>(fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = T>,
{
    match Handle::try_current() {
        Ok(handle) => Ok(task::block_in_place(|| handle.block_on(fut))),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|err| anyhow!("Failed to create Tokio runtime: {err}"))?;
            Ok(rt.block_on(fut))
        }
    }
}
