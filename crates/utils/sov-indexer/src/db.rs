use crate::background_sync::{
    parse_kind_amount_roots, parse_withdraw_attestations, parse_withdraw_recipient,
};
use crate::index_db as idx;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use sea_orm::{
    entity::prelude::*, sea_query::OnConflict, Condition, DatabaseConnection, JsonValue,
    QueryOrder, QuerySelect, Schema, Set,
};
use serde::{Deserialize, Serialize};
use sov_midnight_da::storable::worker_verified_transactions;

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
    // FVK registry table for multi-address decryption support
    let stmt = builder.build(
        &schema
            .create_table_from_entity(idx::fvk_registry::Entity)
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
    pub amount: Option<String>,
    pub anchor_root: Option<String>,
    pub nullifier: Option<String>,
    pub view_fvks: Option<JsonValue>,
    pub view_attestations: Option<JsonValue>,
    pub events: Option<JsonValue>,
    pub status: Option<String>,
    pub encrypted_notes: Option<JsonValue>,
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ListResponse {
    pub items: Vec<InvolvementItem>,
    pub next: Option<String>,
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
    let _ = idx::index_meta::Entity::insert(idx::index_meta::ActiveModel {
        key: Set("last_id".to_string()),
        value: Set(id.to_string()),
    })
    .on_conflict(
        OnConflict::column(idx::index_meta::Column::Key)
            .do_nothing()
            .to_owned(),
    )
    .exec(idx_db)
    .await?;
    let _ = idx::index_meta::Entity::update(idx::index_meta::ActiveModel {
        key: Set("last_id".to_string()),
        value: Set(id.to_string()),
    })
    .exec(idx_db)
    .await;
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
    decrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_deposit::Entity::insert(idx::midnight_deposit::ActiveModel {
        event_id: Set(event_id),
        amount: Set(amount),
        rho: Set(rho),
        recipient: Set(recipient),
        sender: Set(sender),
        view_fvks: Set(view_fvks),
        encrypted_notes: Set(encrypted_notes),
        decrypted_notes: Set(decrypted_notes),
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
    view_attestations: Option<JsonValue>,
    encrypted_notes: Option<JsonValue>,
    decrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_withdraw::Entity::insert(idx::midnight_withdraw::ActiveModel {
        event_id: Set(event_id),
        amount: Set(amount),
        anchor_root: Set(anchor_root),
        nullifier: Set(nullifier),
        to_addr: Set(to),
        sender: Set(sender),
        view_attestations: Set(view_attestations),
        encrypted_notes: Set(encrypted_notes),
        decrypted_notes: Set(decrypted_notes),
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
    recipient: Option<String>,
    view_attestations: Option<JsonValue>,
    encrypted_notes: Option<JsonValue>,
    decrypted_notes: Option<JsonValue>,
) -> Result<()> {
    let _ = idx::midnight_transfer::Entity::insert(idx::midnight_transfer::ActiveModel {
        event_id: Set(event_id),
        anchor_root: Set(anchor_root),
        nullifier: Set(nullifier),
        sender: Set(sender),
        recipient: Set(recipient),
        view_attestations: Set(view_attestations),
        encrypted_notes: Set(encrypted_notes),
        decrypted_notes: Set(decrypted_notes),
    })
    .exec(idx_db)
    .await?;
    Ok(())
}

pub async fn list_wallet_txs_sync(
    db: &DatabaseConnection,
    address: &str,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
) -> Result<ListResponse> {
    let mut collected: Vec<InvolvementItem> = Vec::new();

    // Deposits by sender
    let deps = idx::midnight_deposit::Entity::find()
        .filter(idx::midnight_deposit::Column::Sender.eq(address.to_string()))
        .all(db)
        .await?;
    for md in deps {
        let Some(ev) = idx::Entity::find_by_id(md.event_id).one(db).await? else {
            continue;
        };
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                continue;
            }
        }
        if let Some(ref t) = type_filter {
            if t != "deposit" {
                continue;
            }
        }
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: md.sender.clone(),
            recipient: None,
            amount: md.amount.clone(),
            anchor_root: None,
            nullifier: None,
            view_fvks: md.view_fvks.clone(),
            view_attestations: None,
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes: md.encrypted_notes.clone(),
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }

    // Withdrawals by sender or recipient
    let wds = idx::midnight_withdraw::Entity::find()
        .filter(
            Condition::any()
                .add(idx::midnight_withdraw::Column::Sender.eq(address.to_string()))
                .add(idx::midnight_withdraw::Column::ToAddr.eq(address.to_string())),
        )
        .all(db)
        .await?;
    for mw in wds {
        let Some(ev) = idx::Entity::find_by_id(mw.event_id).one(db).await? else {
            continue;
        };
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                continue;
            }
        }
        if let Some(ref t) = type_filter {
            if t != "withdraw" {
                continue;
            }
        }
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: mw.sender.clone(),
            recipient: mw.to_addr.clone(),
            amount: mw.amount.clone(),
            anchor_root: mw.anchor_root.clone(),
            nullifier: mw.nullifier.clone(),
            view_fvks: None,
            view_attestations: mw.view_attestations.clone(),
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes: mw.encrypted_notes.clone(),
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }

    // Transfers by sender or recipient
    let tfs = idx::midnight_transfer::Entity::find()
        .filter(
            Condition::any()
                .add(idx::midnight_transfer::Column::Sender.eq(address.to_string()))
                .add(idx::midnight_transfer::Column::Recipient.eq(address.to_string())),
        )
        .all(db)
        .await?;
    for mt in tfs {
        let Some(ev) = idx::Entity::find_by_id(mt.event_id).one(db).await? else {
            continue;
        };
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                continue;
            }
        }
        if let Some(ref t) = type_filter {
            if t != "transfer" {
                continue;
            }
        }
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: mt.sender.clone(),
            recipient: mt.recipient.clone(),
            amount: None,
            anchor_root: mt.anchor_root.clone(),
            nullifier: mt.nullifier.clone(),
            view_fvks: None,
            view_attestations: mt.view_attestations.clone(),
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes: mt.encrypted_notes.clone(),
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }

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
    Ok(ListResponse {
        items: collected,
        next,
    })
}

pub async fn list_wallet_txs_direct(
    db: &DatabaseConnection,
    address: &str,
    limit: usize,
    cursor: Option<CursorInner>,
    type_filter: Option<String>,
) -> Result<ListResponse> {
    let mut collected: Vec<InvolvementItem> = Vec::new();

    let mut query = worker_verified_transactions::Entity::find()
        .filter(
            Condition::any()
                .add(worker_verified_transactions::Column::Sender.eq(address.to_string()))
                .add(worker_verified_transactions::Column::Recipient.eq(address.to_string())),
        )
        .order_by_desc(worker_verified_transactions::Column::CreatedAt);

    if let Some(ref cur) = cursor {
        let ts = DateTime::<Utc>::from_timestamp_millis(cur.ts_ms).unwrap();
        query = query.filter(
            Condition::any()
                .add(worker_verified_transactions::Column::CreatedAt.lt(ts))
                .add(
                    worker_verified_transactions::Column::CreatedAt
                        .eq(ts)
                        .and(worker_verified_transactions::Column::TxHash.lt(cur.tx_hash.clone())),
                ),
        );
    }

    let rows = query.limit(limit as u64).all(db).await?;
    for row in rows {
        let (kind, amount, anchor_root, nullifier) = parse_kind_amount_roots(&row.transaction_data)
            .unwrap_or(("other".to_string(), None, None, None));
        if let Some(ref t) = type_filter {
            if t != &kind {
                continue;
            }
        }
        let view_fvks = row
            .view_fvks_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let view_attestations = row
            .view_attestations_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .or_else(|| {
                parse_withdraw_attestations(&row.proof_outputs)
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            });
        let recipient = if kind == "withdraw" {
            row.recipient.clone().or_else(|| {
                parse_withdraw_recipient(&row.transaction_data)
                    .ok()
                    .flatten()
            })
        } else {
            None
        };
        let events = extract_events_from_status(row.sequencer_status.as_deref())
            .ok()
            .flatten();
        let status = extract_status_from_status(row.sequencer_status.as_deref());
        let encrypted_notes = row
            .encrypted_notes_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());

        collected.push(InvolvementItem {
            tx_hash: row.tx_hash.clone(),
            timestamp_ms: row.created_at.timestamp_millis(),
            kind,
            sender: Some(row.sender.clone()),
            recipient,
            amount,
            anchor_root,
            nullifier,
            view_fvks,
            view_attestations,
            events,
            status,
            encrypted_notes,
            payload: serde_json::from_str(&row.transaction_data).ok(),
        });
    }

    let next = collected.last().map(|item| {
        BASE64_STANDARD.encode(
            serde_json::to_vec(&CursorInner {
                ts_ms: item.timestamp_ms,
                tx_hash: item.tx_hash.clone(),
            })
            .unwrap(),
        )
    });
    Ok(ListResponse {
        items: collected,
        next,
    })
}

pub async fn list_txs(
    db: &DatabaseConnection,
    mode: crate::api::Mode,
    limit: usize,
    offset: usize,
) -> Result<ListResponse> {
    if mode == crate::api::Mode::Direct {
        let rows = worker_verified_transactions::Entity::find()
            .order_by_desc(worker_verified_transactions::Column::CreatedAt)
            .offset(offset as u64)
            .limit(limit as u64)
            .all(db)
            .await?;
        let mut items = Vec::new();
        for row in rows {
            let (kind, amount, anchor_root, nullifier) = parse_kind_amount_roots(
                &row.transaction_data,
            )
            .unwrap_or(("other".to_string(), None, None, None));
            let view_fvks = row
                .view_fvks_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let view_attestations = row
                .view_attestations_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .or_else(|| {
                    parse_withdraw_attestations(&row.proof_outputs)
                        .ok()
                        .flatten()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                });
            let recipient = if kind == "withdraw" {
                row.recipient.clone().or_else(|| {
                    parse_withdraw_recipient(&row.transaction_data)
                        .ok()
                        .flatten()
                })
            } else {
                None
            };
            let events = extract_events_from_status(row.sequencer_status.as_deref())
                .ok()
                .flatten();
            let status = extract_status_from_status(row.sequencer_status.as_deref());
            let encrypted_notes = row
                .encrypted_notes_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());

            items.push(InvolvementItem {
                tx_hash: row.tx_hash.clone(),
                timestamp_ms: row.created_at.timestamp_millis(),
                kind,
                sender: Some(row.sender.clone()),
                recipient,
                amount,
                anchor_root,
                nullifier,
                view_fvks,
                view_attestations,
                events,
                status,
                encrypted_notes,
                payload: serde_json::from_str(&row.transaction_data).ok(),
            });
        }
        return Ok(ListResponse { items, next: None });
    }

    // Sync mode
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
            amount,
            anchor_root,
            nullifier,
            view_fvks,
            view_attestations,
            events: ev.events.clone(),
            status: ev.status.clone(),
            encrypted_notes,
            payload: serde_json::from_str(&ev.payload).ok(),
        });
    }
    Ok(ListResponse { items, next: None })
}

pub async fn get_tx(
    db: &DatabaseConnection,
    mode: crate::api::Mode,
    tx_hash: &str,
) -> Result<Option<InvolvementItem>> {
    if mode == crate::api::Mode::Direct {
        let row = worker_verified_transactions::Entity::find()
            .filter(worker_verified_transactions::Column::TxHash.eq(tx_hash.to_string()))
            .one(db)
            .await?;
        if let Some(row) = row {
            let (kind, amount, anchor_root, nullifier) = parse_kind_amount_roots(
                &row.transaction_data,
            )
            .unwrap_or(("other".to_string(), None, None, None));
            let view_fvks = row
                .view_fvks_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let view_attestations = row
                .view_attestations_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .or_else(|| {
                    parse_withdraw_attestations(&row.proof_outputs)
                        .ok()
                        .flatten()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                });
            let recipient = if kind == "withdraw" {
                row.recipient.clone().or_else(|| {
                    parse_withdraw_recipient(&row.transaction_data)
                        .ok()
                        .flatten()
                })
            } else {
                None
            };
            let events = extract_events_from_status(row.sequencer_status.as_deref())
                .ok()
                .flatten();
            let status = extract_status_from_status(row.sequencer_status.as_deref());
            let encrypted_notes = row
                .encrypted_notes_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());

            return Ok(Some(InvolvementItem {
                tx_hash: row.tx_hash.clone(),
                timestamp_ms: row.created_at.timestamp_millis(),
                kind,
                sender: Some(row.sender.clone()),
                recipient,
                amount,
                anchor_root,
                nullifier,
                view_fvks,
                view_attestations,
                events,
                status,
                encrypted_notes,
                payload: serde_json::from_str(&row.transaction_data).ok(),
            }));
        }
        return Ok(None);
    }

    // Sync mode: look up by tx_hash in events then related tables
    let ev = idx::Entity::find()
        .filter(idx::Column::TxHash.eq(tx_hash.to_string()))
        .one(db)
        .await?;
    let Some(ev) = ev else {
        return Ok(None);
    };

    let mut sender = None;
    let mut recipient = None;
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
        amount,
        anchor_root,
        nullifier,
        view_fvks,
        view_attestations,
        events: ev.events.clone(),
        status: ev.status.clone(),
        encrypted_notes,
        payload: serde_json::from_str(&ev.payload).ok(),
    }))
}
