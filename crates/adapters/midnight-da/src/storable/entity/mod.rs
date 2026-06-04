//! [sea-orm](https://www.sea-ql.org/SeaORM/docs/index/) related code.
use sea_orm::sea_query::{Index, IndexCreateStatement};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, QueryOrder, Schema};

use crate::config::GENESIS_HEADER;
use crate::MidnightBlockHeader;

pub mod blobs;
pub mod block_headers;
pub mod finalized_height;
pub mod worker_verified_transactions;

pub(crate) const BATCH_NAMESPACE: &str = "batches";
pub(crate) const PROOF_NAMESPACE: &str = "proofs";

// DB Functions

/// Ensures the schema required for the storable MidnightDA layer exists on the
/// provided database connection.
pub async fn setup_db(db: &DatabaseConnection) -> anyhow::Result<()> {
    tracing::debug!("Setting up database");
    create_tables(db, blobs::Entity).await?;
    create_tables(db, block_headers::Entity).await?;
    create_tables(db, finalized_height::Entity).await?;
    create_tables(db, worker_verified_transactions::Entity).await?;
    let builder = db.get_database_backend();
    let index_stmt: IndexCreateStatement = Index::create()
        .name("idx-blobs-block_height")
        .table(blobs::Entity)
        .col(blobs::Column::BlockHeight)
        .if_not_exists()
        .to_owned();
    db.execute(builder.build(&index_stmt)).await?;
    let verified_tx_idx: IndexCreateStatement = Index::create()
        .name("idx-worker_verified_transactions-tx_hash")
        .table(worker_verified_transactions::Entity)
        .col(worker_verified_transactions::Column::TxHash)
        .unique()
        .if_not_exists()
        .to_owned();
    db.execute(builder.build(&verified_tx_idx)).await?;
    let verified_tx_state_idx: IndexCreateStatement = Index::create()
        .name("idx-worker_verified_transactions-transaction_state")
        .table(worker_verified_transactions::Entity)
        .col(worker_verified_transactions::Column::TransactionState)
        .if_not_exists()
        .to_owned();
    db.execute(builder.build(&verified_tx_state_idx)).await?;
    let verified_tx_state_created_idx: IndexCreateStatement = Index::create()
        .name("idx-worker_verified_transactions-state_created_at")
        .table(worker_verified_transactions::Entity)
        .col(worker_verified_transactions::Column::TransactionState)
        .col(worker_verified_transactions::Column::CreatedAt)
        .if_not_exists()
        .to_owned();
    db.execute(builder.build(&verified_tx_state_created_idx))
        .await?;
    let verified_tx_state_id_idx: IndexCreateStatement = Index::create()
        .name("idx-worker_verified_transactions-state_id")
        .table(worker_verified_transactions::Entity)
        .col(worker_verified_transactions::Column::TransactionState)
        .col(worker_verified_transactions::Column::Id)
        .if_not_exists()
        .to_owned();
    db.execute(builder.build(&verified_tx_state_id_idx)).await?;
    if let DbBackend::Sqlite = db.get_database_backend() {
        // Enable WAL mode for better concurrency
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA journal_mode = WAL".to_owned(),
        ))
        .await?;

        // Set busy timeout to 30 seconds to handle high-concurrency scenarios
        // This prevents immediate "database is locked" errors
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA busy_timeout = 30000".to_owned(),
        ))
        .await?;

        // Increase cache size to 64MB for better performance
        // Negative value means size in KB (64MB = 64 * 1024 KB = 65536 KB)
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA cache_size = -65536".to_owned(),
        ))
        .await?;

        // Use NORMAL synchronous mode for better write performance
        // Still crash-safe with WAL mode, but faster than FULL
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA synchronous = NORMAL".to_owned(),
        ))
        .await?;

        // Increase page size to 8KB for better I/O efficiency with large blobs
        // Note: This only affects new databases; existing ones keep their page size
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA page_size = 8192".to_owned(),
        ))
        .await?;

        // Use memory for temporary storage to speed up complex queries
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA temp_store = MEMORY".to_owned(),
        ))
        .await?;

        // Set mmap_size to 256MB for memory-mapped I/O performance
        db.execute(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA mmap_size = 268435456".to_owned(),
        ))
        .await?;

        tracing::info!("SQLite performance optimizations applied: WAL mode, 30s busy_timeout, 64MB cache, NORMAL sync, 8KB pages, MEMORY temp_store, 256MB mmap");
    }
    Ok(())
}

pub(crate) async fn create_tables<E: EntityTrait>(
    db: &DatabaseConnection,
    entity: E,
) -> anyhow::Result<()> {
    let builder = db.get_database_backend();
    let schema = Schema::new(builder);
    db.execute(
        builder.build(
            &schema
                .create_table_from_entity(entity)
                .if_not_exists()
                .to_owned(),
        ),
    )
    .await?;
    Ok(())
}

pub(crate) async fn query_last_saved_block(
    db: &DatabaseConnection,
) -> anyhow::Result<MidnightBlockHeader> {
    let db_value = block_headers::Entity::find()
        .order_by_desc(block_headers::Column::Height)
        .one(db)
        .await?
        .map(MidnightBlockHeader::from);
    tracing::trace!(?db_value, "Loaded latest block header from database");
    Ok(db_value.unwrap_or(GENESIS_HEADER))
}

pub(crate) async fn query_last_finalized_height(db: &DatabaseConnection) -> anyhow::Result<u32> {
    let db_value = finalized_height::Entity::find_by_id(finalized_height::ID)
        .one(db)
        .await?
        .map(|model| model.value as u32);

    tracing::trace!(finalized_height = ?db_value, "Loaded latest finalized height from database");
    Ok(db_value.unwrap_or_default())
}
