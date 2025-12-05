use crate::db::{list_wallet_txs_direct, list_wallet_txs_sync, CursorInner, ListResponse};
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
use sea_orm::DatabaseConnection;
use serde::Deserialize;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Sync,
    Direct,
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
    let cursor = if let Some(cur) = q.cursor.as_deref() {
        let raw = BASE64_STANDARD.decode(cur)?;
        Some(serde_json::from_slice::<CursorInner>(&raw)?)
    } else {
        None
    };
    if state.mode == Mode::Direct {
        return list_wallet_txs_direct(&state.db, &address, limit, cursor, q.r#type.clone()).await;
    }
    list_wallet_txs_sync(&state.db, &address, limit, cursor, q.r#type.clone()).await
}
