use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::OnConflict;
use sea_orm::{Condition, DatabaseConnection, JsonValue, QueryOrder, QuerySelect, Schema, Set};
use serde::{Deserialize, Serialize};

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
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::index_meta::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    // VFK registry table for multi-address decryption support
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::vfk_registry::Entity)
            .if_not_exists()
            .to_owned(),
    );
    idx_db.execute(stmt).await?;
    Ok(())
}

#[derive(Debug, Serialize, Clone)]
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
    pub view_fvks: Option<JsonValue>,
    pub view_attestations: Option<JsonValue>,
    pub events: Option<JsonValue>,
    pub status: Option<String>,
    pub encrypted_notes: Option<JsonValue>,
    pub decrypted_notes: Option<JsonValue>,
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone)]
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
        decrypted_notes: Set(None),
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
        decrypted_notes: Set(None),
    })
    .exec(idx_db)
    .await?;
    Ok(())
}

pub async fn insert_midnight_transfer(
    idx_db: &DatabaseConnection,
    event_id: i32,
    anchor_root: Option<String>,
    nullifier: Option<String>,
    sender: Option<String>,
    privacy_sender: Option<String>,
    recipient: Option<String>,
    view_attestations: Option<JsonValue>,
    encrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_transfer::Entity::insert(idx::midnight_transfer::ActiveModel {
        event_id: Set(event_id),
        anchor_root: Set(anchor_root),
        nullifier: Set(nullifier),
        sender: Set(sender),
        privacy_sender: Set(privacy_sender),
        recipient: Set(recipient),
        view_attestations: Set(view_attestations),
        encrypted_notes: Set(encrypted_notes),
        decrypted_notes: Set(None),
    })
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
    use midnight_privacy::PrivacyAddress;
    use crate::viewer::hex_to_bech32m_address;

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
    debug!("Fetching midnight_deposit records for sender or recipient={}", normalized_address);
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
        let decrypted_notes = resolve_decrypted_notes(
            vfk,
            md.decrypted_notes.as_ref(),
            md.encrypted_notes.as_ref(),
        );
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
                    ev.tx_hash, t
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
        let decrypted_notes = resolve_decrypted_notes(
            vfk,
            mw.decrypted_notes.as_ref(),
            mw.encrypted_notes.as_ref(),
        );
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
                    ev.tx_hash, t
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
        let decrypted_notes = resolve_decrypted_notes(
            vfk,
            mt.decrypted_notes.as_ref(),
            mt.encrypted_notes.as_ref(),
        );
        let privacy_sender = mt
            .privacy_sender
            .clone()
            .or_else(|| viewer::extract_sender_from_decrypted_notes(decrypted_notes.as_ref()));
        let privacy_recipient = mt
            .recipient
            .clone()
            .or_else(|| viewer::extract_recipient_from_decrypted_notes(decrypted_notes.as_ref()));
        let matches_recipient =
            privacy_recipient.as_deref() == Some(normalized_address.as_str())
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
                    ev.tx_hash, t
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

pub async fn get_tx(
    db: &DatabaseConnection,
    tx_hash: &str,
) -> Result<Option<InvolvementItem>> {
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

fn resolve_decrypted_notes(
    vfk: Option<&Hash32>,
    _stored: Option<&JsonValue>,
    encrypted: Option<&JsonValue>,
) -> Option<JsonValue> {
    let Some(vfk) = vfk else {
        return None;
    };
    viewer::try_decrypt_notes_json(vfk, encrypted)
}

fn decrypted_notes_match_recipient(
    decrypted_notes: Option<&JsonValue>,
    recipient: &str,
) -> bool {
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
