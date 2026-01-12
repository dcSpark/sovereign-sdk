use crate::db::{list_wallet_txs_direct, list_wallet_txs_sync, CursorInner, ListResponse};
use crate::viewer::{self, VfkRegistry};
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
    Json, Router,
};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    pub mode: Mode,
    /// VFK registry using DashMap for lock-free concurrent access
    pub vfk_registry: Arc<VfkRegistry>,
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

#[derive(Debug, Deserialize)]
pub struct TxListQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/wallets/:address", get(list_wallet_txs))
        .route("/txs/:tx_hash", get(get_tx))
        .route("/txs", get(list_txs))
        .route("/health", get(health))
        // VFK registry management endpoints
        .route("/vfks", get(list_vfks).post(add_vfk))
        .route("/vfks/:fvk_commitment", delete(delete_vfk))
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

async fn get_tx(Path(tx_hash): Path<String>, State(state): State<AppState>) -> impl IntoResponse {
    match crate::db::get_tx(&state.db, state.mode, &tx_hash).await {
        Ok(Some(item)) => (StatusCode::OK, Json(item)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list_txs(
    Query(q): Query<TxListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let limit = q.limit.min(200);
    let offset = q.offset;
    match crate::db::list_txs(&state.db, state.mode, limit, offset).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============== VFK Registry Management ==============

/// Request body for adding a new VFK
#[derive(Debug, Deserialize)]
pub struct AddVfkRequest {
    /// The VFK as hex string (32 bytes = 64 hex chars)
    pub vfk: String,
    /// Optional: expected fvk_commitment (hex) - if provided, we verify it matches
    #[serde(default)]
    pub fvk_commitment: Option<String>,
    /// The shielded address associated with this VFK (optional)
    #[serde(default)]
    pub shielded_address: Option<String>,
}

/// Response for VFK operations
#[derive(Debug, Serialize)]
pub struct VfkResponse {
    pub fvk_commitment: String,
    pub vfk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
}

/// List all VFKs in the registry
async fn list_vfks(State(state): State<AppState>) -> impl IntoResponse {
    let vfks: Vec<VfkResponse> = state
        .vfk_registry
        .entries()
        .into_iter()
        .map(|(commitment, vfk, addr)| VfkResponse {
            fvk_commitment: commitment,
            vfk: hex::encode(vfk),
            shielded_address: addr,
        })
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "count": vfks.len(),
            "vfks": vfks
        })),
    )
}

/// Add a new VFK to the registry
async fn add_vfk(
    State(state): State<AppState>,
    Json(req): Json<AddVfkRequest>,
) -> impl IntoResponse {
    // Parse the VFK
    let vfk = match viewer::parse_vfk_hex(&req.vfk) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid VFK: {}", e)})),
            )
                .into_response();
        }
    };

    // Compute the commitment from the VFK
    let vfk_obj = midnight_privacy::FullViewingKey(vfk);
    let commitment = midnight_privacy::viewing::fvk_commitment(&vfk_obj);
    let commitment_hex = hex::encode(commitment);

    // If user provided an expected fvk_commitment, verify it matches
    if let Some(ref expected) = req.fvk_commitment {
        let expected_normalized = expected.trim().to_lowercase();
        let expected_normalized = expected_normalized
            .strip_prefix("0x")
            .unwrap_or(&expected_normalized);

        if expected_normalized != commitment_hex {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "fvk_commitment mismatch",
                    "expected": expected_normalized,
                    "computed": commitment_hex,
                    "message": "The provided fvk_commitment does not match the commitment computed from the VFK"
                })),
            )
                .into_response();
        }
    }

    // Check if this VFK already exists in the registry
    if state.vfk_registry.get_vfk(&commitment_hex).is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "VFK already exists",
                "fvk_commitment": commitment_hex,
                "message": "A VFK with this commitment is already registered"
            })),
        )
            .into_response();
    }

    // Add to registry (DashMap - no lock needed)
    state.vfk_registry.add(vfk, req.shielded_address.clone());

    // Persist to database
    if let Err(e) = state.vfk_registry.save_to_db(&state.db).await {
        tracing::warn!("Failed to persist VFK to database: {}", e);
    }

    tracing::info!("Added VFK with commitment {}", &commitment_hex[..16]);

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "success": true,
            "fvk_commitment": commitment_hex,
            "message": "VFK added and verified successfully"
        })),
    )
        .into_response()
}

/// Delete a VFK from the registry
async fn delete_vfk(
    Path(fvk_commitment): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    use crate::index_db as idx;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    // Check if it exists and remove (DashMap - no lock needed)
    if !state.vfk_registry.remove(&fvk_commitment) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "VFK not found"})),
        )
            .into_response();
    }

    // Remove from database
    if let Err(e) = idx::vfk_registry::Entity::delete_many()
        .filter(idx::vfk_registry::Column::FvkCommitment.eq(&fvk_commitment))
        .exec(&state.db)
        .await
    {
        tracing::warn!("Failed to delete VFK from database: {}", e);
    }

    tracing::info!(
        "Deleted VFK with commitment {}",
        &fvk_commitment[..16.min(fvk_commitment.len())]
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "message": "VFK deleted successfully"
        })),
    )
        .into_response()
}
