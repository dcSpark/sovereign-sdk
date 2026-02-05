use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement, OnConflict};
use sea_orm::{
    Condition, DatabaseBackend, DatabaseConnection, JsonValue, QueryOrder, QuerySelect, Schema,
    Set, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::index_db as idx;
use crate::viewer;
use midnight_privacy::Hash32;

pub async fn init_index_db(idx_db: &DatabaseConnection) -> Result<()> {
    let builder = idx_db.get_database_backend();
    let schema = Schema::new(builder);
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::midnight_deposit::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::midnight_withdraw::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::midnight_transfer::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;

    // Flattened nullifier set (for multi-input transfers).
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::midnight_spent_nullifiers::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::index_meta::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    // FVK registry table for multi-address decryption support
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::fvk_registry::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;

    // Prefunded wallets table for external wallet assignment (e.g. MCP)
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::prefunded_wallets::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;

    // UTXO-like note tracking table (auditor/indexer view)
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::notes_nullifiers::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;

    // Frozen accounts tracking table (freeze/unfreeze history with reasons)
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::frozen_accounts::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;

    // === Indexes ===
    //
    // These are additive (no migrations). We create them with IF NOT EXISTS so existing DBs
    // can pick them up on restart.

    // Cursor pagination: ORDER BY created_at DESC, tie-breaker tx_hash.
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_events_created_at_tx_hash")
        .table(idx::Entity)
        .col(idx::Column::CreatedAt)
        .col(idx::Column::TxHash)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    // Optional type filter + cursor pagination.
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_events_kind_created_at_tx_hash")
        .table(idx::Entity)
        .col(idx::Column::Kind)
        .col(idx::Column::CreatedAt)
        .col(idx::Column::TxHash)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    // Wallet filters (privacy side).
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_deposit_recipient")
        .table(idx::midnight_deposit::Entity)
        .col(idx::midnight_deposit::Column::Recipient)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_deposit_sender")
        .table(idx::midnight_deposit::Entity)
        .col(idx::midnight_deposit::Column::Sender)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_transfer_recipient")
        .table(idx::midnight_transfer::Entity)
        .col(idx::midnight_transfer::Column::Recipient)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_transfer_privacy_sender")
        .table(idx::midnight_transfer::Entity)
        .col(idx::midnight_transfer::Column::PrivacySender)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_transfer_sender")
        .table(idx::midnight_transfer::Entity)
        .col(idx::midnight_transfer::Column::Sender)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_withdraw_privacy_sender")
        .table(idx::midnight_withdraw::Entity)
        .col(idx::midnight_withdraw::Column::PrivacySender)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_withdraw_sender")
        .table(idx::midnight_withdraw::Entity)
        .col(idx::midnight_withdraw::Column::Sender)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_midnight_withdraw_to_addr")
        .table(idx::midnight_withdraw::Entity)
        .col(idx::midnight_withdraw::Column::ToAddr)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    // UTXO-style note tracking: common query is recipient + unspent.
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_notes_nullifiers_recipient")
        .table(idx::notes_nullifiers::Entity)
        .col(idx::notes_nullifiers::Column::Recipient)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_notes_nullifiers_recipient_spent_tx_hash")
        .table(idx::notes_nullifiers::Entity)
        .col(idx::notes_nullifiers::Column::Recipient)
        .col(idx::notes_nullifiers::Column::SpentTxHash)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    // Common time-range queries (e.g., median tx size) filter by kind + created_at.
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_notes_nullifiers_created_kind_created_at")
        .table(idx::notes_nullifiers::Entity)
        .col(idx::notes_nullifiers::Column::CreatedKind)
        .col(idx::notes_nullifiers::Column::CreatedAt)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    // Frozen accounts: quick lookup by privacy address
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_frozen_accounts_privacy_address")
        .table(idx::frozen_accounts::Entity)
        .col(idx::frozen_accounts::Column::PrivacyAddress)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;
    // Frozen accounts: quick lookup by wallet address
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_frozen_accounts_wallet_address")
        .table(idx::frozen_accounts::Entity)
        .col(idx::frozen_accounts::Column::WalletAddress)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    // Prefunded wallets: speed up claiming of available wallets
    let idx_stmt: IndexCreateStatement = Index::create()
        .name("idx_prefunded_wallets_used_created_at")
        .table(idx::prefunded_wallets::Entity)
        .col(idx::prefunded_wallets::Column::Used)
        .col(idx::prefunded_wallets::Column::CreatedAt)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&idx_stmt)).await?;

    Ok(())
}

pub async fn reset_index_db(idx_db: &DatabaseConnection) -> Result<()> {
    let backend = idx_db.get_database_backend();
    set_foreign_key_checks(idx_db, backend, false).await?;
    let tables = list_all_tables(idx_db, backend).await?;
    for table in tables {
        if table == "fvk_registry" || table == "prefunded_wallets" {
            continue;
        }
        let quoted = quote_table(&table, backend);
        let sql = match backend {
            DatabaseBackend::Postgres => format!("DROP TABLE IF EXISTS {} CASCADE", quoted),
            _ => format!("DROP TABLE IF EXISTS {}", quoted),
        };
        idx_db.execute(Statement::from_string(backend, sql)).await?;
    }
    set_foreign_key_checks(idx_db, backend, true).await?;
    Ok(())
}

async fn set_foreign_key_checks(
    idx_db: &DatabaseConnection,
    backend: DatabaseBackend,
    enabled: bool,
) -> Result<()> {
    let sql = match backend {
        DatabaseBackend::Sqlite => {
            if enabled {
                "PRAGMA foreign_keys = ON"
            } else {
                "PRAGMA foreign_keys = OFF"
            }
        }
        DatabaseBackend::MySql => {
            if enabled {
                "SET FOREIGN_KEY_CHECKS = 1"
            } else {
                "SET FOREIGN_KEY_CHECKS = 0"
            }
        }
        DatabaseBackend::Postgres => return Ok(()),
    };
    idx_db.execute(Statement::from_string(backend, sql)).await?;
    Ok(())
}

async fn list_all_tables(
    idx_db: &DatabaseConnection,
    backend: DatabaseBackend,
) -> Result<Vec<String>> {
    let sql = match backend {
        DatabaseBackend::Sqlite => {
            "SELECT name AS name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        }
        DatabaseBackend::Postgres => {
            "SELECT tablename AS name FROM pg_tables WHERE schemaname = current_schema()"
        }
        DatabaseBackend::MySql => {
            "SELECT table_name AS name FROM information_schema.tables WHERE table_schema = DATABASE()"
        }
    };
    let rows = idx_db
        .query_all(Statement::from_string(backend, sql))
        .await?;
    let mut tables = Vec::new();
    for row in rows {
        if let Ok(name) = row.try_get::<String>("", "name") {
            tables.push(name);
        }
    }
    Ok(tables)
}

fn quote_table(table: &str, backend: DatabaseBackend) -> String {
    match backend {
        DatabaseBackend::MySql => format!("`{}`", table),
        _ => format!("\"{}\"", table),
    }
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct InvolvementItem {
    pub tx_hash: String,
    pub timestamp_ms: i64,
    pub kind: String,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub privacy_sender: Option<String>,
    pub privacy_recipient: Option<String>,
    pub amount: Option<String>,
    pub anchor_root: Option<String>,
    pub nullifier: Option<String>,
    #[schema(value_type = Value)]
    pub view_fvks: Option<JsonValue>,
    #[schema(value_type = Value)]
    pub view_attestations: Option<JsonValue>,
    #[schema(value_type = Value)]
    pub events: Option<JsonValue>,
    pub status: Option<String>,
    #[schema(value_type = Value)]
    pub encrypted_notes: Option<JsonValue>,
    #[schema(value_type = Value)]
    pub decrypted_notes: Option<JsonValue>,
    #[schema(value_type = Value)]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ListResponse {
    pub items: Vec<InvolvementItem>,
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CursorInner {
    pub ts_ms: i64,
    pub tx_hash: String,
}

pub fn after_cursor(cur: &CursorInner, created_at: DateTime<Utc>, tx_hash: &str) -> bool {
    let ts = DateTime::<Utc>::from_timestamp_millis(cur.ts_ms).unwrap();
    created_at < ts || (created_at == ts && tx_hash < cur.tx_hash.as_str())
}

pub fn extract_events_from_status(
    sequencer_resp: Option<&str>,
) -> Result<Option<serde_json::Value>, serde_json::Error> {
    if let Some(resp) = sequencer_resp {
        let v: serde_json::Value = serde_json::from_str(resp)?;
        if let Some(ev) = v.get("events") {
            return Ok(Some(ev.clone()));
        }
    }
    Ok(None)
}

pub fn extract_status_from_status(sequencer_resp: Option<&str>) -> Option<String> {
    if let Some(resp) = sequencer_resp {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(resp) {
            if let Some(r) = v
                .get("receipt")
                .and_then(|r| r.get("result"))
                .and_then(|s| s.as_str())
            {
                return Some(r.to_string());
            }
            if let Some(s) = v.get("status").and_then(|s| s.as_str()) {
                return Some(s.to_string());
            }
        }
    }
    None
}

pub async fn get_last_processed_id(idx_db: &DatabaseConnection) -> Result<Option<i32>> {
    let row = idx::index_meta::Entity::find_by_id("last_id")
        .one(idx_db)
        .await?;
    Ok(row.map(|m| m.value.parse().unwrap_or(0)))
}

pub async fn set_last_processed_id(idx_db: &DatabaseConnection, id: i32) -> Result<()> {
    idx::index_meta::Entity::insert(idx::index_meta::ActiveModel {
        key: Set("last_id".to_string()),
        value: Set(id.to_string()),
    })
    .on_conflict(
        OnConflict::column(idx::index_meta::Column::Key)
            .update_column(idx::index_meta::Column::Value)
            .to_owned(),
    )
    .exec(idx_db)
    .await
    .inspect_err(|e| tracing::warn!(error = %e, id = id, "failed to upsert last_processed_id"))?;

    Ok(())
}

pub async fn get_index_meta(idx_db: &DatabaseConnection, key: &str) -> Result<Option<String>> {
    let row = idx::index_meta::Entity::find_by_id(key).one(idx_db).await?;
    Ok(row.map(|m| m.value))
}

pub async fn set_index_meta(idx_db: &DatabaseConnection, key: &str, value: &str) -> Result<()> {
    idx::index_meta::Entity::insert(idx::index_meta::ActiveModel {
        key: Set(key.to_string()),
        value: Set(value.to_string()),
    })
    .on_conflict(
        OnConflict::column(idx::index_meta::Column::Key)
            .update_column(idx::index_meta::Column::Value)
            .to_owned(),
    )
    .exec(idx_db)
    .await?;
    Ok(())
}

pub async fn insert_event(
    idx_db: &DatabaseConnection,
    tx_hash: &str,
    created_at: DateTime<Utc>,
    module: &str,
    kind: &str,
    payload: &str,
    status: Option<String>,
    events: Option<JsonValue>,
) -> Result<Option<i32>> {
    let res = idx::Entity::insert(idx::ActiveModel {
        tx_hash: Set(tx_hash.to_string()),
        created_at: Set(created_at),
        module: Set(module.to_string()),
        kind: Set(kind.to_string()),
        status: Set(status),
        events: Set(events),
        payload: Set(payload.to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(idx::Column::TxHash)
            .do_nothing()
            .to_owned(),
    )
    .exec(idx_db)
    .await;

    match res {
        Ok(r) => Ok(Some(r.last_insert_id)),
        Err(sea_orm::DbErr::RecordNotInserted) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub async fn insert_midnight_deposit(
    idx_db: &DatabaseConnection,
    event_id: i32,
    amount: Option<String>,
    rho: Option<String>,
    recipient: Option<String>,
    sender: Option<String>,
    view_fvks: Option<JsonValue>,
    encrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_deposit::Entity::insert(idx::midnight_deposit::ActiveModel {
        event_id: Set(event_id),
        amount: Set(amount),
        rho: Set(rho),
        recipient: Set(recipient),
        sender: Set(sender),
        view_fvks: Set(view_fvks),
        encrypted_notes: Set(encrypted_notes),
    })
    .exec(idx_db)
    .await?;
    Ok(())
}

pub async fn insert_midnight_withdraw(
    idx_db: &DatabaseConnection,
    event_id: i32,
    amount: Option<String>,
    anchor_root: Option<String>,
    nullifier: Option<String>,
    to: Option<String>,
    sender: Option<String>,
    privacy_sender: Option<String>,
    view_attestations: Option<JsonValue>,
    encrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_withdraw::Entity::insert(idx::midnight_withdraw::ActiveModel {
        event_id: Set(event_id),
        amount: Set(amount),
        anchor_root: Set(anchor_root),
        nullifier: Set(nullifier),
        to_addr: Set(to),
        sender: Set(sender),
        privacy_sender: Set(privacy_sender),
        view_attestations: Set(view_attestations),
        encrypted_notes: Set(encrypted_notes),
    })
    .exec(idx_db)
    .await?;
    Ok(())
}

pub async fn insert_midnight_transfer(
    idx_db: &DatabaseConnection,
    event_id: i32,
    amount: Option<String>,
    anchor_root: Option<String>,
    nullifier: Option<String>,
    sender: Option<String>,
    privacy_sender: Option<String>,
    recipient: Option<String>,
    view_attestations: Option<JsonValue>,
    encrypted_notes: Option<JsonValue>,
    decrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_transfer::Entity::insert(idx::midnight_transfer::ActiveModel {
        event_id: Set(event_id),
        amount: Set(amount),
        anchor_root: Set(anchor_root),
        nullifier: Set(nullifier),
        sender: Set(sender),
        privacy_sender: Set(privacy_sender),
        recipient: Set(recipient),
        view_attestations: Set(view_attestations),
        encrypted_notes: Set(encrypted_notes),
        decrypted_notes: Set(decrypted_notes),
    })
    .exec(idx_db)
    .await?;
    Ok(())
}

fn normalize_hash32_hex(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let trimmed = trimmed.to_lowercase();
    anyhow::ensure!(
        trimmed.len() == 64,
        "expected 32-byte hex (64 chars), got {} chars",
        trimmed.len()
    );
    let bytes = hex::decode(&trimmed)?;
    anyhow::ensure!(bytes.len() == 32, "expected 32 bytes, got {}", bytes.len());
    Ok(trimmed)
}

fn normalize_privacy_addr_bech32m(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    let hrp_prefix = format!("{}1", viewer::PRIVACY_ADDRESS_HRP);
    if lower.starts_with(&hrp_prefix) {
        return Ok(lower);
    }
    viewer::hex_to_bech32m_address(&lower).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid privacy address/recipient hash: expected 32-byte hex or {}..., got {}",
            hrp_prefix,
            trimmed
        )
    })
}

pub async fn upsert_note_created_metadata(
    idx_db: &DatabaseConnection,
    cm: &str,
    created_tx_hash: &str,
    created_at: DateTime<Utc>,
    created_kind: &str,
) -> Result<()> {
    let cm = normalize_hash32_hex(cm)?;
    idx::notes_nullifiers::Entity::insert(idx::notes_nullifiers::ActiveModel {
        cm: Set(cm),
        created_tx_hash: Set(Some(created_tx_hash.to_string())),
        created_at: Set(Some(created_at)),
        created_kind: Set(Some(created_kind.to_string())),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(idx::notes_nullifiers::Column::Cm)
            .update_columns([
                idx::notes_nullifiers::Column::CreatedTxHash,
                idx::notes_nullifiers::Column::CreatedAt,
                idx::notes_nullifiers::Column::CreatedKind,
            ])
            .to_owned(),
    )
    .exec(idx_db)
    .await?;
    Ok(())
}

pub async fn upsert_note_created(
    idx_db: &DatabaseConnection,
    cm: &str,
    domain: Option<&str>,
    value: Option<&str>,
    rho: Option<&str>,
    recipient: Option<&str>,
    sender_id: Option<&str>,
    cm_ins: Option<JsonValue>,
    created_tx_hash: Option<&str>,
    created_at: Option<DateTime<Utc>>,
    created_kind: Option<&str>,
) -> Result<()> {
    let cm = normalize_hash32_hex(cm)?;
    let domain = match domain {
        Some(v) => Some(normalize_hash32_hex(v)?),
        None => None,
    };
    let rho = match rho {
        Some(v) => Some(normalize_hash32_hex(v)?),
        None => None,
    };
    let recipient = match recipient {
        Some(v) => Some(normalize_privacy_addr_bech32m(v)?),
        None => None,
    };
    let sender_id = match sender_id {
        Some(v) => Some(normalize_privacy_addr_bech32m(v)?),
        None => None,
    };

    idx::notes_nullifiers::Entity::insert(idx::notes_nullifiers::ActiveModel {
        cm: Set(cm),
        domain: Set(domain),
        value: Set(value.map(|s| s.to_string())),
        rho: Set(rho),
        recipient: Set(recipient),
        sender_id: Set(sender_id),
        cm_ins: Set(cm_ins),
        created_tx_hash: Set(created_tx_hash.map(|s| s.to_string())),
        created_at: Set(created_at),
        created_kind: Set(created_kind.map(|s| s.to_string())),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(idx::notes_nullifiers::Column::Cm)
            .update_columns([
                idx::notes_nullifiers::Column::Domain,
                idx::notes_nullifiers::Column::Value,
                idx::notes_nullifiers::Column::Rho,
                idx::notes_nullifiers::Column::Recipient,
                idx::notes_nullifiers::Column::SenderId,
                idx::notes_nullifiers::Column::CmIns,
                idx::notes_nullifiers::Column::CreatedTxHash,
                idx::notes_nullifiers::Column::CreatedAt,
                idx::notes_nullifiers::Column::CreatedKind,
            ])
            .to_owned(),
    )
    .exec(idx_db)
    .await?;

    Ok(())
}

pub async fn upsert_note_spent(
    idx_db: &DatabaseConnection,
    cm: &str,
    spent_tx_hash: Option<&str>,
    spent_at: Option<DateTime<Utc>>,
    spent_nullifier: Option<&str>,
    spent_kind: Option<&str>,
) -> Result<()> {
    let cm = normalize_hash32_hex(cm)?;
    let spent_nullifier = match spent_nullifier {
        Some(v) => Some(normalize_hash32_hex(v)?),
        None => None,
    };

    idx::notes_nullifiers::Entity::insert(idx::notes_nullifiers::ActiveModel {
        cm: Set(cm),
        spent_tx_hash: Set(spent_tx_hash.map(|s| s.to_string())),
        spent_at: Set(spent_at),
        spent_nullifier: Set(spent_nullifier),
        spent_kind: Set(spent_kind.map(|s| s.to_string())),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(idx::notes_nullifiers::Column::Cm)
            .update_columns([
                idx::notes_nullifiers::Column::SpentTxHash,
                idx::notes_nullifiers::Column::SpentAt,
                idx::notes_nullifiers::Column::SpentNullifier,
                idx::notes_nullifiers::Column::SpentKind,
            ])
            .to_owned(),
    )
    .exec(idx_db)
    .await?;

    Ok(())
}

pub async fn upsert_spent_nullifier(
    idx_db: &DatabaseConnection,
    nullifier: &str,
    spent_tx_hash: &str,
    spent_at: DateTime<Utc>,
    spent_kind: &str,
) -> Result<()> {
    let nullifier = normalize_hash32_hex(nullifier)?;
    idx::midnight_spent_nullifiers::Entity::insert(idx::midnight_spent_nullifiers::ActiveModel {
        nullifier: Set(nullifier),
        spent_tx_hash: Set(spent_tx_hash.to_string()),
        spent_at: Set(spent_at),
        spent_kind: Set(spent_kind.to_string()),
    })
    .on_conflict(
        OnConflict::column(idx::midnight_spent_nullifiers::Column::Nullifier)
            .update_columns([
                idx::midnight_spent_nullifiers::Column::SpentTxHash,
                idx::midnight_spent_nullifiers::Column::SpentAt,
                idx::midnight_spent_nullifiers::Column::SpentKind,
            ])
            .to_owned(),
    )
    .exec(idx_db)
    .await?;
    Ok(())
}

/// Convert a privacy address to recipient hash if needed.
///
/// If the address is a long privacy address (privpool1... with >50 chars),
/// it extracts the keys and computes the recipient hash, then encodes it as bech32m.
/// Otherwise returns the address as-is.
fn normalize_address_for_query(address: &str) -> String {
    use crate::viewer::hex_to_bech32m_address;
    use midnight_privacy::PrivacyAddress;

    // Domain constant used for privacy operations (matches the rest of the codebase)
    const DOMAIN: [u8; 32] = [1u8; 32];

    // If it's a long privacy address (>50 chars), convert to recipient hash
    if address.starts_with("privpool1") && address.len() > 50 {
        // Try to parse as PrivacyAddress
        if let Ok(privacy_addr) = address.parse::<PrivacyAddress>() {
            let domain = DOMAIN;

            // Compute recipient hash
            let pk_spend = privacy_addr.to_pk();
            let pk_ivk = privacy_addr.pk_ivk();
            let recipient = midnight_privacy::recipient_from_pk_v2(&domain, &pk_spend, &pk_ivk);

            // Encode as bech32m
            let recipient_hex = hex::encode(recipient);
            if let Some(bech32_addr) = hex_to_bech32m_address(&recipient_hex) {
                tracing::debug!(
                    "Converted long privacy address {} to recipient hash {}",
                    address,
                    bech32_addr
                );
                return bech32_addr;
            }
        }
    }

    address.to_string()
}

pub async fn list_wallet_txs(
    db: &DatabaseConnection,
    address: &str,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
    vfk: Option<Hash32>,
) -> Result<ListResponse> {
    use tracing::{debug, info, trace};

    let mut collected: Vec<InvolvementItem> = Vec::new();
    info!(
        "Starting list_wallet_txs for address {}, limit {}, cursor {:?}, type_filter {:?}",
        address, limit, cursor, type_filter
    );

    // Normalize address: convert long privacy address to recipient hash if needed
    let normalized_address = normalize_address_for_query(address);
    debug!("Normalized address: {} -> {}", address, normalized_address);
    let vfk = vfk.as_ref();
    let include_encrypted = vfk.is_none();
    let address_is_privacy = address.starts_with("privpool1");
    let should_scan_privacy = vfk.is_some() && address_is_privacy;
    let mut deposit_matches = 0u64;
    let mut withdraw_matches = 0u64;
    let mut transfer_matches = 0u64;

    // Deposits by sender OR recipient
    debug!(
        "Fetching midnight_deposit records for sender or recipient={}",
        normalized_address
    );
    let deposit_filter = if should_scan_privacy {
        Condition::any()
            .add(idx::midnight_deposit::Column::Sender.eq(address.to_string()))
            .add(idx::midnight_deposit::Column::Recipient.eq(normalized_address.clone()))
            .add(
                Condition::all()
                    .add(idx::midnight_deposit::Column::Recipient.is_null())
                    .add(idx::midnight_deposit::Column::EncryptedNotes.is_not_null()),
            )
    } else {
        Condition::any()
            .add(idx::midnight_deposit::Column::Sender.eq(address.to_string()))
            .add(idx::midnight_deposit::Column::Recipient.eq(normalized_address.clone()))
    };
    let deps = idx::midnight_deposit::Entity::find()
        .filter(deposit_filter)
        .all(db)
        .await?;
    debug!("Got {} midnight_deposit records", deps.len());

    for md in deps.iter() {
        trace!("Processing deposit event_id {}", md.event_id);
        let Some(ev) = idx::Entity::find_by_id(md.event_id).one(db).await? else {
            trace!("Deposit event id {} not found in idx::Entity", md.event_id);
            continue;
        };
        let decrypted_notes = resolve_decrypted_notes(vfk, md.encrypted_notes.as_ref());
        let privacy_recipient = md
            .recipient
            .clone()
            .or_else(|| viewer::extract_recipient_from_decrypted_notes(decrypted_notes.as_ref()));
        let matches = if address_is_privacy {
            privacy_recipient.as_deref() == Some(normalized_address.as_str())
                || decrypted_notes_match_recipient(decrypted_notes.as_ref(), &normalized_address)
        } else {
            md.sender.as_deref() == Some(address)
        };
        if matches {
            deposit_matches += 1;
        }
        if !matches {
            continue;
        }
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                trace!(
                    "Deposit {} not after cursor (ts={}, tx_hash={}), skipping",
                    ev.tx_hash,
                    ev.created_at.timestamp_millis(),
                    ev.tx_hash
                );
                continue;
            }
        }
        if let Some(ref t) = type_filter {
            if t != "deposit" {
                trace!(
                    "Deposit {} filtered out by type (expected deposit, got {})",
                    ev.tx_hash,
                    t
                );
                continue;
            }
        }
        debug!(
            "Adding deposit involvement item for tx_hash={} event_id={}",
            ev.tx_hash, md.event_id
        );
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: md.sender.clone(),
            recipient: privacy_recipient.clone(),
            privacy_sender: None,
            privacy_recipient,
            amount: md.amount.clone(),
            anchor_root: None,
            nullifier: None,
            view_fvks: md.view_fvks.clone(),
            view_attestations: None,
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes: if include_encrypted {
                md.encrypted_notes.clone()
            } else {
                None
            },
            decrypted_notes,
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }

    // Withdrawals by sender or recipient
    debug!(
        "Fetching midnight_withdraw records for sender or recipient={}",
        address
    );
    let mut withdraw_filter = Condition::any()
        .add(idx::midnight_withdraw::Column::Sender.eq(address.to_string()))
        .add(idx::midnight_withdraw::Column::ToAddr.eq(address.to_string()));
    if address_is_privacy {
        withdraw_filter = withdraw_filter
            .add(idx::midnight_withdraw::Column::PrivacySender.eq(normalized_address.clone()));
        if should_scan_privacy {
            withdraw_filter = withdraw_filter.add(
                Condition::all()
                    .add(idx::midnight_withdraw::Column::PrivacySender.is_null())
                    .add(idx::midnight_withdraw::Column::EncryptedNotes.is_not_null()),
            );
        }
    }
    let wds = idx::midnight_withdraw::Entity::find()
        .filter(withdraw_filter)
        .all(db)
        .await?;
    debug!("Got {} midnight_withdraw records", wds.len());
    for mw in wds.iter() {
        trace!("Processing withdraw event_id {}", mw.event_id);
        let Some(ev) = idx::Entity::find_by_id(mw.event_id).one(db).await? else {
            trace!("Withdraw event id {} not found in idx::Entity", mw.event_id);
            continue;
        };
        let decrypted_notes = resolve_decrypted_notes(vfk, mw.encrypted_notes.as_ref());
        let privacy_sender = mw
            .privacy_sender
            .clone()
            .or_else(|| viewer::extract_sender_from_decrypted_notes(decrypted_notes.as_ref()));
        let matches = if address_is_privacy {
            privacy_sender.as_deref() == Some(normalized_address.as_str())
        } else {
            mw.sender.as_deref() == Some(address) || mw.to_addr.as_deref() == Some(address)
        };
        if matches {
            withdraw_matches += 1;
        }
        if !matches {
            continue;
        }
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                trace!(
                    "Withdraw {} not after cursor (ts={}, tx_hash={}), skipping",
                    ev.tx_hash,
                    ev.created_at.timestamp_millis(),
                    ev.tx_hash
                );
                continue;
            }
        }
        if let Some(ref t) = type_filter {
            if t != "withdraw" {
                trace!(
                    "Withdraw {} filtered out by type (expected withdraw, got {})",
                    ev.tx_hash,
                    t
                );
                continue;
            }
        }
        debug!(
            "Adding withdraw involvement item for tx_hash={} event_id={}",
            ev.tx_hash, mw.event_id
        );
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: mw.sender.clone(),
            recipient: mw.to_addr.clone(),
            privacy_sender,
            privacy_recipient: None,
            amount: mw.amount.clone(),
            anchor_root: mw.anchor_root.clone(),
            nullifier: mw.nullifier.clone(),
            view_fvks: None,
            view_attestations: mw.view_attestations.clone(),
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes: if include_encrypted {
                mw.encrypted_notes.clone()
            } else {
                None
            },
            decrypted_notes,
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }

    // Transfers by sender or recipient
    debug!(
        "Fetching midnight_transfer records for sender or recipient={}",
        normalized_address
    );
    let mut transfer_filter = Condition::any()
        .add(idx::midnight_transfer::Column::Sender.eq(address.to_string()))
        .add(idx::midnight_transfer::Column::Recipient.eq(normalized_address.clone()))
        .add(idx::midnight_transfer::Column::PrivacySender.eq(normalized_address.clone()));
    if should_scan_privacy {
        transfer_filter = transfer_filter.add(
            Condition::all()
                .add(idx::midnight_transfer::Column::EncryptedNotes.is_not_null())
                .add(
                    Condition::any()
                        .add(idx::midnight_transfer::Column::Recipient.is_null())
                        .add(idx::midnight_transfer::Column::PrivacySender.is_null()),
                ),
        );
    }
    let tfs = idx::midnight_transfer::Entity::find()
        .filter(transfer_filter)
        .all(db)
        .await?;
    debug!("Got {} midnight_transfer records", tfs.len());
    for mt in tfs.iter() {
        trace!("Processing transfer event_id {}", mt.event_id);
        let Some(ev) = idx::Entity::find_by_id(mt.event_id).one(db).await? else {
            trace!("Transfer event id {} not found in idx::Entity", mt.event_id);
            continue;
        };
        let decrypted_notes = resolve_decrypted_notes(vfk, mt.encrypted_notes.as_ref());
        let privacy_sender = mt
            .privacy_sender
            .clone()
            .or_else(|| viewer::extract_sender_from_decrypted_notes(decrypted_notes.as_ref()));
        let privacy_recipient = mt
            .recipient
            .clone()
            .or_else(|| viewer::extract_recipient_from_decrypted_notes(decrypted_notes.as_ref()));
        let matches_recipient = privacy_recipient.as_deref() == Some(normalized_address.as_str())
            || decrypted_notes_match_recipient(decrypted_notes.as_ref(), &normalized_address);
        let matches_sender = privacy_sender.as_deref() == Some(normalized_address.as_str());
        let matches = if address_is_privacy {
            matches_recipient || matches_sender
        } else {
            mt.sender.as_deref() == Some(address)
        };
        if matches {
            transfer_matches += 1;
        }
        if !matches {
            trace!(
                "Transfer {} does not match address {}, skipping",
                ev.tx_hash,
                normalized_address
            );
            continue;
        }
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                trace!(
                    "Transfer {} not after cursor (ts={}, tx_hash={}), skipping",
                    ev.tx_hash,
                    ev.created_at.timestamp_millis(),
                    ev.tx_hash
                );
                continue;
            }
        }
        if let Some(ref t) = type_filter {
            if t != "transfer" {
                trace!(
                    "Transfer {} filtered out by type (expected transfer, got {})",
                    ev.tx_hash,
                    t
                );
                continue;
            }
        }
        debug!(
            "Adding transfer involvement item for tx_hash={} event_id={}",
            ev.tx_hash, mt.event_id
        );
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: mt.sender.clone(),
            recipient: privacy_recipient.clone(),
            privacy_sender,
            privacy_recipient,
            amount: None,
            anchor_root: mt.anchor_root.clone(),
            nullifier: mt.nullifier.clone(),
            view_fvks: None,
            view_attestations: mt.view_attestations.clone(),
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes: if include_encrypted {
                mt.encrypted_notes.clone()
            } else {
                None
            },
            decrypted_notes,
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }

    let total = match type_filter.as_deref() {
        Some("deposit") => deposit_matches,
        Some("withdraw") => withdraw_matches,
        Some("transfer") => transfer_matches,
        Some(_) => 0,
        None => deposit_matches + withdraw_matches + transfer_matches,
    };

    debug!(
        "Sorting and truncating {} collected records",
        collected.len()
    );
    collected.sort_by(|a, b| {
        b.timestamp_ms
            .cmp(&a.timestamp_ms)
            .then(b.tx_hash.cmp(&a.tx_hash))
    });
    collected.truncate(limit);

    let next = collected.last().map(|item| {
        BASE64_STANDARD.encode(
            serde_json::to_vec(&CursorInner {
                ts_ms: item.timestamp_ms,
                tx_hash: item.tx_hash.clone(),
            })
            .unwrap(),
        )
    });
    info!(
        "Finished list_wallet_txs: returning {} items, next cursor {:?}",
        collected.len(),
        next
    );
    Ok(ListResponse {
        items: collected,
        next,
        total: Some(total),
    })
}

pub async fn list_txs(
    db: &DatabaseConnection,
    limit: usize,
    offset: usize,
) -> Result<ListResponse> {
    let rows = idx::Entity::find()
        .order_by_desc(idx::Column::CreatedAt)
        .offset(offset as u64)
        .limit(limit as u64)
        .all(db)
        .await?;
    let mut items = Vec::new();
    for ev in rows {
        let mut sender = None;
        let mut recipient = None;
        let mut privacy_sender = None;
        let mut privacy_recipient = None;
        let mut amount = None;
        let mut anchor_root = None;
        let mut nullifier = None;
        let mut view_fvks = None;
        let mut view_attestations = None;
        let mut encrypted_notes = None;
        match ev.kind.as_str() {
            "deposit" => {
                if let Some(md) = idx::midnight_deposit::Entity::find_by_id(ev.id)
                    .one(db)
                    .await?
                {
                    sender = md.sender;
                    recipient = md.recipient;
                    privacy_recipient = recipient.clone();
                    amount = md.amount;
                    view_fvks = md.view_fvks;
                    encrypted_notes = md.encrypted_notes;
                }
            }
            "withdraw" => {
                if let Some(mw) = idx::midnight_withdraw::Entity::find_by_id(ev.id)
                    .one(db)
                    .await?
                {
                    sender = mw.sender;
                    recipient = mw.to_addr;
                    privacy_sender = mw.privacy_sender;
                    amount = mw.amount;
                    anchor_root = mw.anchor_root;
                    nullifier = mw.nullifier;
                    view_attestations = mw.view_attestations;
                    encrypted_notes = mw.encrypted_notes;
                }
            }
            "transfer" => {
                if let Some(mt) = idx::midnight_transfer::Entity::find_by_id(ev.id)
                    .one(db)
                    .await?
                {
                    sender = mt.sender;
                    recipient = mt.recipient;
                    privacy_sender = mt.privacy_sender;
                    privacy_recipient = recipient.clone();
                    amount = mt.amount;
                    anchor_root = mt.anchor_root;
                    nullifier = mt.nullifier;
                    view_attestations = mt.view_attestations;
                    encrypted_notes = mt.encrypted_notes;
                }
            }
            _ => {}
        }

        items.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender,
            recipient,
            privacy_sender,
            privacy_recipient,
            amount,
            anchor_root,
            nullifier,
            view_fvks,
            view_attestations,
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes,
            decrypted_notes: None,
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }
    Ok(ListResponse {
        items,
        next: None,
        total: None,
    })
}

pub async fn get_tx(db: &DatabaseConnection, tx_hash: &str) -> Result<Option<InvolvementItem>> {
    // Look up by tx_hash in events then related tables.
    let ev = idx::Entity::find()
        .filter(idx::Column::TxHash.eq(tx_hash.to_string()))
        .one(db)
        .await?;
    let Some(ev) = ev else {
        return Ok(None);
    };

    let mut sender = None;
    let mut recipient = None;
    let mut privacy_sender = None;
    let mut privacy_recipient = None;
    let mut amount = None;
    let mut anchor_root = None;
    let mut nullifier = None;
    let mut view_fvks = None;
    let mut view_attestations = None;
    let mut encrypted_notes = None;

    match ev.kind.as_str() {
        "deposit" => {
            if let Some(md) = idx::midnight_deposit::Entity::find_by_id(ev.id)
                .one(db)
                .await?
            {
                sender = md.sender;
                recipient = md.recipient;
                privacy_recipient = recipient.clone();
                amount = md.amount;
                view_fvks = md.view_fvks;
                encrypted_notes = md.encrypted_notes;
            }
        }
        "withdraw" => {
            if let Some(mw) = idx::midnight_withdraw::Entity::find_by_id(ev.id)
                .one(db)
                .await?
            {
                sender = mw.sender;
                recipient = mw.to_addr;
                privacy_sender = mw.privacy_sender;
                amount = mw.amount;
                anchor_root = mw.anchor_root;
                nullifier = mw.nullifier;
                view_attestations = mw.view_attestations;
                encrypted_notes = mw.encrypted_notes;
            }
        }
        "transfer" => {
            if let Some(mt) = idx::midnight_transfer::Entity::find_by_id(ev.id)
                .one(db)
                .await?
            {
                sender = mt.sender;
                recipient = mt.recipient;
                privacy_sender = mt.privacy_sender;
                privacy_recipient = recipient.clone();
                amount = mt.amount;
                anchor_root = mt.anchor_root;
                nullifier = mt.nullifier;
                view_attestations = mt.view_attestations;
                encrypted_notes = mt.encrypted_notes;
            }
        }
        _ => {}
    }

    Ok(Some(InvolvementItem {
        tx_hash: ev.tx_hash.clone(),
        timestamp_ms: ev.created_at.timestamp_millis(),
        kind: ev.kind.clone(),
        sender,
        recipient,
        privacy_sender,
        privacy_recipient,
        amount,
        anchor_root,
        nullifier,
        view_fvks,
        view_attestations,
        events: ev.events.clone(),
        status: ev.status.clone(),
        encrypted_notes,
        decrypted_notes: None,
        payload: serde_json::from_str(&ev.payload).ok(),
    }))
}

pub async fn get_tx_god(db: &DatabaseConnection, tx_hash: &str) -> Result<Option<InvolvementItem>> {
    let ev = idx::Entity::find()
        .filter(idx::Column::TxHash.eq(tx_hash.to_string()))
        .one(db)
        .await?;
    let Some(ev) = ev else {
        return Ok(None);
    };

    let item = build_transaction_item(db, &ev, true).await?;
    Ok(Some(item))
}

fn resolve_decrypted_notes(
    vfk: Option<&Hash32>,
    encrypted: Option<&JsonValue>,
) -> Option<JsonValue> {
    let Some(vfk) = vfk else {
        return None;
    };
    viewer::try_decrypt_notes_json(vfk, encrypted)
}

/// List all transactions with pagination (public mode - hides privacy-sensitive fields).
/// Uses cursor-based pagination for efficiency.
pub async fn list_transactions(
    db: &DatabaseConnection,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
) -> Result<ListResponse> {
    list_transactions_internal(db, limit, cursor, type_filter, false).await
}

/// List all transactions with pagination (god mode - shows all fields including decrypted data).
/// Uses cursor-based pagination for efficiency.
pub async fn list_transactions_god(
    db: &DatabaseConnection,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
) -> Result<ListResponse> {
    list_transactions_internal(db, limit, cursor, type_filter, true).await
}

async fn list_transactions_internal(
    db: &DatabaseConnection,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
    god_mode: bool,
) -> Result<ListResponse> {
    use tracing::debug;

    let mut query = idx::Entity::find().order_by_desc(idx::Column::CreatedAt);

    if let Some(ref kind) = type_filter {
        query = query.filter(idx::Column::Kind.eq(kind.to_string()));
    }

    // Apply cursor filter for efficient pagination
    if let Some(ref cur) = cursor {
        let cursor_ts =
            DateTime::<Utc>::from_timestamp_millis(cur.ts_ms).unwrap_or_else(|| Utc::now());
        // Fetch records where (created_at < cursor_ts) OR (created_at == cursor_ts AND tx_hash < cursor_tx_hash)
        query = query.filter(
            Condition::any()
                .add(idx::Column::CreatedAt.lt(cursor_ts))
                .add(
                    Condition::all()
                        .add(idx::Column::CreatedAt.eq(cursor_ts))
                        .add(idx::Column::TxHash.lt(cur.tx_hash.clone())),
                ),
        );
    }

    // Fetch limit + 1 to determine if there are more results
    let rows = query.limit((limit + 1) as u64).all(db).await?;

    let has_more = rows.len() > limit;
    let rows: Vec<_> = rows.into_iter().take(limit).collect();

    debug!(
        "list_transactions: fetched {} events, has_more={}",
        rows.len(),
        has_more
    );

    let mut items = Vec::new();
    for ev in &rows {
        let item = build_transaction_item(db, ev, god_mode).await?;
        items.push(item);
    }

    // Build next cursor from last item
    let next = if has_more {
        items.last().map(|item| {
            BASE64_STANDARD.encode(
                serde_json::to_vec(&CursorInner {
                    ts_ms: item.timestamp_ms,
                    tx_hash: item.tx_hash.clone(),
                })
                .unwrap(),
            )
        })
    } else {
        None
    };

    Ok(ListResponse {
        items,
        next,
        total: None,
    })
}

/// List transactions filtered by wallet address (public mode - hides privacy-sensitive fields).
/// Uses cursor-based pagination for efficiency.
pub async fn list_wallet_transactions(
    db: &DatabaseConnection,
    address: &str,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
) -> Result<ListResponse> {
    list_wallet_transactions_internal(db, address, limit, cursor, type_filter, false).await
}

/// List transactions filtered by wallet address (god mode - shows all fields).
/// Uses cursor-based pagination for efficiency.
pub async fn list_wallet_transactions_god(
    db: &DatabaseConnection,
    address: &str,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
) -> Result<ListResponse> {
    list_wallet_transactions_internal(db, address, limit, cursor, type_filter, true).await
}

async fn list_wallet_transactions_internal(
    db: &DatabaseConnection,
    address: &str,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
    god_mode: bool,
) -> Result<ListResponse> {
    use tracing::{debug, trace};

    let normalized_address = normalize_address_for_query(address);
    let address_is_privacy = address.starts_with("privpool1");
    let cursor_filter = cursor.as_ref().map(|cur| {
        let cursor_ts = DateTime::<Utc>::from_timestamp_millis(cur.ts_ms).unwrap_or_else(Utc::now);
        // Fetch records where (created_at < cursor_ts) OR (created_at == cursor_ts AND tx_hash < cursor_tx_hash)
        Condition::any()
            .add(idx::Column::CreatedAt.lt(cursor_ts))
            .add(
                Condition::all()
                    .add(idx::Column::CreatedAt.eq(cursor_ts))
                    .add(idx::Column::TxHash.lt(cur.tx_hash.clone())),
            )
    });

    debug!(
        "list_wallet_transactions: address={}, normalized={}, is_privacy={}, god_mode={}",
        address, normalized_address, address_is_privacy, god_mode
    );

    // We need to query all three tables and merge results
    // To make this efficient with cursor pagination, we:
    // 1. Query each table separately with the cursor filter
    // 2. Merge, sort, and take the top `limit` items

    let mut collected: Vec<InvolvementItem> = Vec::new();
    let per_kind_limit = (limit + 1) as u64;

    // Deposits
    if type_filter.is_none() || type_filter.as_deref() == Some("deposit") {
        let deposit_filter = if address_is_privacy {
            Condition::any()
                .add(idx::midnight_deposit::Column::Recipient.eq(normalized_address.clone()))
        } else {
            Condition::any().add(idx::midnight_deposit::Column::Sender.eq(address.to_string()))
        };

        let mut deps_query = idx::midnight_deposit::Entity::find()
            .filter(deposit_filter)
            .find_also_related(idx::Entity)
            .order_by_desc(idx::Column::CreatedAt)
            .order_by_desc(idx::Column::TxHash)
            .limit(per_kind_limit);
        if let Some(ref cond) = cursor_filter {
            deps_query = deps_query.filter(cond.clone());
        }
        let deps = deps_query.all(db).await?;

        trace!("Found {} deposit records for wallet", deps.len());

        for (md, ev) in deps {
            let Some(ev) = ev else {
                continue;
            };

            collected.push(InvolvementItem {
                tx_hash: ev.tx_hash.clone(),
                timestamp_ms: ev.created_at.timestamp_millis(),
                kind: ev.kind.clone(),
                sender: md.sender.clone(),
                recipient: if god_mode { md.recipient.clone() } else { None },
                privacy_sender: None,
                privacy_recipient: if god_mode { md.recipient.clone() } else { None },
                amount: md.amount.clone(),
                anchor_root: None,
                nullifier: None,
                view_fvks: md.view_fvks.clone(),
                view_attestations: None,
                events: ev.events.clone(),
                status: ev.status.clone(),
                encrypted_notes: md.encrypted_notes.clone(),
                decrypted_notes: None,
                payload: serde_json::from_str(&ev.payload).ok(),
            });
        }
    }

    // Withdrawals
    if type_filter.is_none() || type_filter.as_deref() == Some("withdraw") {
        let withdraw_filter = if address_is_privacy {
            Condition::any()
                .add(idx::midnight_withdraw::Column::PrivacySender.eq(normalized_address.clone()))
        } else {
            Condition::any()
                .add(idx::midnight_withdraw::Column::Sender.eq(address.to_string()))
                .add(idx::midnight_withdraw::Column::ToAddr.eq(address.to_string()))
        };

        let mut wds_query = idx::midnight_withdraw::Entity::find()
            .filter(withdraw_filter)
            .find_also_related(idx::Entity)
            .order_by_desc(idx::Column::CreatedAt)
            .order_by_desc(idx::Column::TxHash)
            .limit(per_kind_limit);
        if let Some(ref cond) = cursor_filter {
            wds_query = wds_query.filter(cond.clone());
        }
        let wds = wds_query.all(db).await?;

        trace!("Found {} withdraw records for wallet", wds.len());

        for (mw, ev) in wds {
            let Some(ev) = ev else {
                continue;
            };

            collected.push(InvolvementItem {
                tx_hash: ev.tx_hash.clone(),
                timestamp_ms: ev.created_at.timestamp_millis(),
                kind: ev.kind.clone(),
                sender: mw.sender.clone(),
                recipient: mw.to_addr.clone(),
                privacy_sender: if god_mode {
                    mw.privacy_sender.clone()
                } else {
                    None
                },
                privacy_recipient: None,
                amount: mw.amount.clone(),
                anchor_root: mw.anchor_root.clone(),
                nullifier: mw.nullifier.clone(),
                view_fvks: None,
                view_attestations: mw.view_attestations.clone(),
                events: ev.events.clone(),
                status: ev.status.clone(),
                encrypted_notes: mw.encrypted_notes.clone(),
                decrypted_notes: None,
                payload: serde_json::from_str(&ev.payload).ok(),
            });
        }
    }

    // Transfers
    if type_filter.is_none() || type_filter.as_deref() == Some("transfer") {
        let transfer_filter = if address_is_privacy {
            Condition::any()
                .add(idx::midnight_transfer::Column::Recipient.eq(normalized_address.clone()))
                .add(idx::midnight_transfer::Column::PrivacySender.eq(normalized_address.clone()))
        } else {
            Condition::any().add(idx::midnight_transfer::Column::Sender.eq(address.to_string()))
        };

        let mut tfs_query = idx::midnight_transfer::Entity::find()
            .filter(transfer_filter)
            .find_also_related(idx::Entity)
            .order_by_desc(idx::Column::CreatedAt)
            .order_by_desc(idx::Column::TxHash)
            .limit(per_kind_limit);
        if let Some(ref cond) = cursor_filter {
            tfs_query = tfs_query.filter(cond.clone());
        }
        let tfs = tfs_query.all(db).await?;

        trace!("Found {} transfer records for wallet", tfs.len());

        for (mt, ev) in tfs {
            let Some(ev) = ev else {
                continue;
            };

            collected.push(InvolvementItem {
                tx_hash: ev.tx_hash.clone(),
                timestamp_ms: ev.created_at.timestamp_millis(),
                kind: ev.kind.clone(),
                sender: mt.sender.clone(),
                recipient: if god_mode { mt.recipient.clone() } else { None },
                privacy_sender: if god_mode {
                    mt.privacy_sender.clone()
                } else {
                    None
                },
                privacy_recipient: if god_mode { mt.recipient.clone() } else { None },
                amount: if god_mode { mt.amount.clone() } else { None },
                anchor_root: mt.anchor_root.clone(),
                nullifier: mt.nullifier.clone(),
                view_fvks: None,
                view_attestations: mt.view_attestations.clone(),
                events: ev.events.clone(),
                status: ev.status.clone(),
                encrypted_notes: mt.encrypted_notes.clone(),
                decrypted_notes: if god_mode {
                    mt.decrypted_notes.clone()
                } else {
                    None
                },
                payload: serde_json::from_str(&ev.payload).ok(),
            });
        }
    }

    // Sort by timestamp descending, then tx_hash descending
    collected.sort_by(|a, b| {
        b.timestamp_ms
            .cmp(&a.timestamp_ms)
            .then(b.tx_hash.cmp(&a.tx_hash))
    });

    // Take limit + 1 to check if there are more
    let has_more = collected.len() > limit;
    collected.truncate(limit);

    // Build next cursor
    let next = if has_more {
        collected.last().map(|item| {
            BASE64_STANDARD.encode(
                serde_json::to_vec(&CursorInner {
                    ts_ms: item.timestamp_ms,
                    tx_hash: item.tx_hash.clone(),
                })
                .unwrap(),
            )
        })
    } else {
        None
    };

    debug!(
        "list_wallet_transactions: returning {} items, has_more={}",
        collected.len(),
        has_more
    );

    Ok(ListResponse {
        items: collected,
        next,
        total: None,
    })
}

/// Build a transaction item from an event row.
/// If god_mode is false, privacy-sensitive fields are hidden.
async fn build_transaction_item(
    db: &DatabaseConnection,
    ev: &idx::Model,
    god_mode: bool,
) -> Result<InvolvementItem> {
    let mut sender = None;
    let mut recipient = None;
    let mut privacy_sender = None;
    let mut privacy_recipient = None;
    let mut amount = None;
    let mut anchor_root = None;
    let mut nullifier = None;
    let mut view_fvks = None;
    let mut view_attestations = None;
    let mut encrypted_notes = None;
    let mut decrypted_notes = None;

    match ev.kind.as_str() {
        "deposit" => {
            if let Some(md) = idx::midnight_deposit::Entity::find_by_id(ev.id)
                .one(db)
                .await?
            {
                sender = md.sender;
                amount = md.amount;
                view_fvks = md.view_fvks;
                encrypted_notes = md.encrypted_notes;
                // Only show recipient (privacy address) in god mode
                if god_mode {
                    recipient = md.recipient.clone();
                    privacy_recipient = md.recipient;
                }
            }
        }
        "withdraw" => {
            if let Some(mw) = idx::midnight_withdraw::Entity::find_by_id(ev.id)
                .one(db)
                .await?
            {
                sender = mw.sender;
                recipient = mw.to_addr; // L2 address, always visible
                amount = mw.amount;
                anchor_root = mw.anchor_root;
                nullifier = mw.nullifier;
                view_attestations = mw.view_attestations;
                encrypted_notes = mw.encrypted_notes;
                // Only show privacy_sender in god mode
                if god_mode {
                    privacy_sender = mw.privacy_sender;
                }
            }
        }
        "transfer" => {
            if let Some(mt) = idx::midnight_transfer::Entity::find_by_id(ev.id)
                .one(db)
                .await?
            {
                sender = mt.sender;
                anchor_root = mt.anchor_root;
                nullifier = mt.nullifier;
                view_attestations = mt.view_attestations;
                encrypted_notes = mt.encrypted_notes;
                // Only show privacy fields, amount, and decrypted notes in god mode
                if god_mode {
                    amount = mt.amount;
                    recipient = mt.recipient.clone();
                    privacy_sender = mt.privacy_sender;
                    privacy_recipient = mt.recipient;
                    decrypted_notes = mt.decrypted_notes;
                }
            }
        }
        _ => {}
    }

    Ok(InvolvementItem {
        tx_hash: ev.tx_hash.clone(),
        timestamp_ms: ev.created_at.timestamp_millis(),
        kind: ev.kind.clone(),
        sender,
        recipient,
        privacy_sender,
        privacy_recipient,
        amount,
        anchor_root,
        nullifier,
        view_fvks,
        view_attestations,
        events: ev.events.clone(),
        status: ev.status.clone(),
        encrypted_notes,
        decrypted_notes,
        payload: serde_json::from_str(&ev.payload).ok(),
    })
}

fn decrypted_notes_match_recipient(decrypted_notes: Option<&JsonValue>, recipient: &str) -> bool {
    let Some(decrypted_notes) = decrypted_notes else {
        return false;
    };
    let notes: Vec<viewer::DecryptedNote> =
        serde_json::from_value(decrypted_notes.clone()).unwrap_or_default();
    for note in notes {
        if let Some(bech32_recipient) = viewer::hex_to_bech32m_address(&note.recipient) {
            if bech32_recipient == recipient {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// Frozen Accounts
// ============================================================================

/// Record a freeze or unfreeze event for a privacy address
pub async fn record_freeze_event(
    db: &DatabaseConnection,
    privacy_address: &str,
    wallet_address: Option<&str>,
    reason: Option<&str>,
    is_frozen: bool,
    tx_hash: Option<&str>,
    initiated_by: Option<&str>,
) -> Result<i64> {
    let model = idx::frozen_accounts::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        privacy_address: Set(privacy_address.to_string()),
        wallet_address: Set(wallet_address.map(|s| s.to_string())),
        reason: Set(reason.map(|s| s.to_string())),
        is_frozen: Set(is_frozen),
        tx_hash: Set(tx_hash.map(|s| s.to_string())),
        initiated_by: Set(initiated_by.map(|s| s.to_string())),
        created_at: Set(Utc::now()),
    };

    let result = idx::frozen_accounts::Entity::insert(model).exec(db).await?;

    Ok(result.last_insert_id)
}

/// Get the current frozen status for a privacy address (most recent event)
pub async fn get_frozen_status(
    db: &DatabaseConnection,
    privacy_address: &str,
) -> Result<Option<FrozenAccountStatus>> {
    let row = idx::frozen_accounts::Entity::find()
        .filter(idx::frozen_accounts::Column::PrivacyAddress.eq(privacy_address))
        .order_by_desc(idx::frozen_accounts::Column::CreatedAt)
        .one(db)
        .await?;

    Ok(row.map(|r| FrozenAccountStatus {
        privacy_address: r.privacy_address,
        wallet_address: r.wallet_address,
        is_frozen: r.is_frozen,
        reason: r.reason,
        tx_hash: r.tx_hash,
        initiated_by: r.initiated_by,
        updated_at: r.created_at,
    }))
}

/// List all currently frozen accounts (latest status = frozen)
pub async fn list_frozen_accounts(
    db: &DatabaseConnection,
    limit: Option<u64>,
) -> Result<Vec<FrozenAccountStatus>> {
    // Get distinct privacy addresses with their most recent freeze event
    // This is a subquery approach: for each address, get the most recent event

    // Get all addresses that have at least one event
    let all_events = idx::frozen_accounts::Entity::find()
        .order_by_desc(idx::frozen_accounts::Column::CreatedAt)
        .all(db)
        .await?;

    // Group by privacy_address and take only the most recent event per address
    let mut latest_by_address: std::collections::HashMap<String, idx::frozen_accounts::Model> =
        std::collections::HashMap::new();
    for event in all_events {
        latest_by_address
            .entry(event.privacy_address.clone())
            .or_insert(event);
    }

    // Filter to only currently frozen accounts
    let mut frozen: Vec<FrozenAccountStatus> = latest_by_address
        .into_values()
        .filter(|e| e.is_frozen)
        .map(|r| FrozenAccountStatus {
            privacy_address: r.privacy_address,
            wallet_address: r.wallet_address,
            is_frozen: r.is_frozen,
            reason: r.reason,
            tx_hash: r.tx_hash,
            initiated_by: r.initiated_by,
            updated_at: r.created_at,
        })
        .collect();

    // Sort by most recently frozen
    frozen.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    if let Some(limit) = limit {
        frozen.truncate(limit as usize);
    }

    Ok(frozen)
}

/// Get freeze/unfreeze history for a privacy address
pub async fn get_freeze_history(
    db: &DatabaseConnection,
    privacy_address: &str,
) -> Result<Vec<FreezeEvent>> {
    let rows = idx::frozen_accounts::Entity::find()
        .filter(idx::frozen_accounts::Column::PrivacyAddress.eq(privacy_address))
        .order_by_desc(idx::frozen_accounts::Column::CreatedAt)
        .all(db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| FreezeEvent {
            id: r.id,
            privacy_address: r.privacy_address,
            wallet_address: r.wallet_address,
            reason: r.reason,
            is_frozen: r.is_frozen,
            tx_hash: r.tx_hash,
            initiated_by: r.initiated_by,
            created_at: r.created_at,
        })
        .collect())
}

/// Current frozen status for an account
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrozenAccountStatus {
    pub privacy_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    pub is_frozen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initiated_by: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// A single freeze/unfreeze event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreezeEvent {
    pub id: i64,
    pub privacy_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub is_frozen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initiated_by: Option<String>,
    pub created_at: DateTime<Utc>,
}
