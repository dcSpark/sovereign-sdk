use crate::index_db as idx;
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub cursor: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
}
fn default_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct InvolvementItem {
    pub tx_hash: String,
    pub timestamp_ms: i64,
    pub kind: String,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub amount: Option<String>,
    pub anchor_root: Option<String>,
    pub nullifier: Option<String>,
    pub view_fvks: Option<serde_json::Value>,
    pub view_attestations: Option<serde_json::Value>,
    pub events: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<InvolvementItem>,
    pub next: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CursorInner {
    ts_ms: i64,
    tx_hash: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/wallets/:address/txs", get(list_wallet_txs))
        .route("/health", get(health))
        .with_state(state)
}

async fn list_wallet_txs(
    Path(address): Path<String>,
    Query(q): Query<ListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match list_wallet_txs_inner(address, q, state).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status":"ok"})))
}

async fn list_wallet_txs_inner(
    address: String,
    q: ListQuery,
    state: AppState,
) -> Result<ListResponse> {
    let limit = q.limit.min(200);
    let mut collected: Vec<InvolvementItem> = Vec::new();
    let cursor = if let Some(cur) = q.cursor.as_deref() {
        let raw = BASE64_STANDARD.decode(cur)?;
        Some(serde_json::from_slice::<CursorInner>(&raw)?)
    } else {
        None
    };

    // Deposits by sender
    let deps = idx::midnight_deposit::Entity::find()
        .filter(idx::midnight_deposit::Column::Sender.eq(address.clone()))
        .all(&state.db)
        .await?;
    for md in deps {
        let Some(ev) = idx::Entity::find_by_id(md.event_id).one(&state.db).await? else { continue };
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                continue;
            }
        }
        if let Some(ref t) = q.r#type {
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
        });
    }
    // Withdrawals by sender or recipient
    let wds = idx::midnight_withdraw::Entity::find()
        .filter(
            Condition::any()
                .add(idx::midnight_withdraw::Column::Sender.eq(address.clone()))
                .add(idx::midnight_withdraw::Column::ToAddr.eq(address.clone())),
        )
        .all(&state.db)
        .await?;
    for mw in wds {
        let Some(ev) = idx::Entity::find_by_id(mw.event_id).one(&state.db).await? else { continue };
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                continue;
            }
        }
        if let Some(ref t) = q.r#type {
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
        });
    }
    // Transfers by sender
    let tfs = idx::midnight_transfer::Entity::find()
        .filter(idx::midnight_transfer::Column::Sender.eq(address.clone()))
        .all(&state.db)
        .await?;
    for mt in tfs {
        let Some(ev) = idx::Entity::find_by_id(mt.event_id).one(&state.db).await? else { continue };
        if let Some(ref cur) = cursor {
            if !after_cursor(cur, ev.created_at, &ev.tx_hash) {
                continue;
            }
        }
        if let Some(ref t) = q.r#type {
            if t != "transfer" {
                continue;
            }
        }
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            sender: mt.sender.clone(),
            recipient: None,
            amount: None,
            anchor_root: mt.anchor_root.clone(),
            nullifier: mt.nullifier.clone(),
            view_fvks: None,
            view_attestations: mt.view_attestations.clone(),
            events: ev.events.clone(),
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

fn after_cursor(cur: &CursorInner, created_at: DateTime<Utc>, tx_hash: &str) -> bool {
    let ts = DateTime::<Utc>::from_timestamp_millis(cur.ts_ms).unwrap();
    created_at < ts || (created_at == ts && tx_hash < cur.tx_hash.as_str())
}
