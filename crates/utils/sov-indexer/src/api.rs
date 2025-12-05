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
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
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
    pub direction: String,
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
    id: i32,
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
    let mut collected = Vec::with_capacity(limit);
    let (cursor_ts, cursor_id) = if let Some(cur) = q.cursor.as_deref() {
        let raw = BASE64_STANDARD.decode(cur)?;
        let inner: CursorInner = serde_json::from_slice(&raw)?;
        (
            Some(DateTime::<Utc>::from_timestamp_millis(inner.ts_ms).unwrap()),
            Some(inner.id),
        )
    } else {
        (None, None)
    };

    let mut query = idx::involvement::Entity::find()
        .filter(idx::involvement::Column::Address.eq(address.clone()))
        .order_by_desc(idx::involvement::Column::Id);

    if let Some(ref t) = q.r#type {
        query = query.filter(idx::Column::Kind.eq(t.clone()));
    }
    if let (Some(_ts), Some(id)) = (cursor_ts, cursor_id) {
        // Since we didn't manually join events here, we paginate only by involvement.id.
        // events.created_at is still returned via the related entity for the cursor we emit.
        let cond = Condition::any().add(idx::involvement::Column::Id.lt(id));
        query = query.filter(cond);
    }

    let rows = query
        .find_also_related(idx::Entity)
        .order_by_desc(idx::Column::CreatedAt)
        .limit(limit as u64)
        .all(&state.db)
        .await?;
    for (inv, ev_opt) in rows.iter() {
        let Some(ev) = ev_opt else {
            continue;
        };
        let (
            mut sender,
            mut recipient,
            mut amount,
            mut anchor_root,
            mut nullifier,
            mut view_fvks,
            mut view_attestations,
        ) = (None, None, None, None, None, None, None);
        let events = ev.events.clone();
        if ev.kind == "deposit" {
            if let Some(md) = idx::midnight_deposit::Entity::find_by_id(ev.id)
                .one(&state.db)
                .await?
            {
                sender = md.sender;
                amount = md.amount;
                view_fvks = md.view_fvks;
            }
        } else if ev.kind == "withdraw" {
            if let Some(mw) = idx::midnight_withdraw::Entity::find_by_id(ev.id)
                .one(&state.db)
                .await?
            {
                sender = mw.sender;
                recipient = mw.to_addr;
                amount = mw.amount;
                anchor_root = mw.anchor_root;
                nullifier = mw.nullifier;
                view_attestations = mw.view_attestations;
            }
        } else if ev.kind == "transfer" {
            if let Some(mt) = idx::midnight_transfer::Entity::find_by_id(ev.id)
                .one(&state.db)
                .await?
            {
                sender = mt.sender;
                anchor_root = mt.anchor_root;
                nullifier = mt.nullifier;
                view_attestations = mt.view_attestations;
            }
        }
        collected.push(InvolvementItem {
            tx_hash: ev.tx_hash.clone(),
            timestamp_ms: ev.created_at.timestamp_millis(),
            kind: ev.kind.clone(),
            direction: inv.direction.clone(),
            sender,
            recipient,
            amount,
            anchor_root,
            nullifier,
            view_fvks,
            view_attestations,
            events,
        });
    }

    let next = if let Some((inv, ev_opt)) = rows.last() {
        if let Some(ev) = ev_opt {
            Some(BASE64_STANDARD.encode(serde_json::to_vec(&CursorInner {
                ts_ms: ev.created_at.timestamp_millis(),
                id: inv.id,
            })?))
        } else {
            None
        }
    } else {
        None
    };
    Ok(ListResponse {
        items: collected,
        next,
    })
}
