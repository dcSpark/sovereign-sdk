use crate::balance;
use crate::db::{list_wallet_txs as list_wallet_txs_db, CursorInner, ListResponse};
use crate::viewer::{self, FvkRegistry};
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use midnight_privacy::Hash32;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    /// VFK registry using DashMap for lock-free concurrent access
    pub vfk_registry: Arc<FvkRegistry>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub cursor: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VfkBody {
    #[serde(default)]
    pub vfk: Option<String>,
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
        .route("/wallets/:address", post(list_wallet_txs))
        .route("/wallets/:address/balance", post(wallet_balance))
        .route("/txs/:tx_hash", get(get_tx))
        .route("/txs", get(list_txs))
        .route("/health", get(health))
        // FVK registry management endpoints
        .route("/fvks", get(list_fvks).post(add_fvk))
        .route("/fvks/:fvk_commitment", delete(delete_fvk))
        .with_state(state)
}

async fn list_wallet_txs(
    Path(address): Path<String>,
    Query(q): Query<ListQuery>,
    State(state): State<AppState>,
    body: Option<Json<VfkBody>>,
) -> impl IntoResponse {
    let vfk = match body.and_then(|Json(body)| body.vfk) {
        Some(vfk_hex) => match viewer::parse_fvk_hex(&vfk_hex) {
            Ok(vfk) => Some(vfk),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("Invalid VFK: {}", e)})),
                )
                    .into_response();
            }
        },
        None => None,
    };

    match list_wallet_txs_inner(address, q.limit, q.cursor, q.r#type, state, vfk).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn wallet_balance(
    Path(address): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<balance::BalanceRequest>,
) -> impl IntoResponse {
    match balance::get_wallet_balance(&state.db, &address, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            let status = if is_balance_client_error(&e) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                status,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status":"ok"})))
}

async fn list_wallet_txs_inner(
    address: String,
    limit: usize,
    cursor: Option<String>,
    type_filter: Option<String>,
    state: AppState,
    vfk: Option<Hash32>,
) -> Result<ListResponse> {
    let limit = limit.min(200);
    let cursor = decode_cursor(cursor)?;
    list_wallet_txs_db(&state.db, &address, limit, cursor, type_filter, vfk).await
}

fn decode_cursor(cursor: Option<String>) -> Result<Option<CursorInner>> {
    let Some(cur) = cursor else {
        return Ok(None);
    };
    let raw = BASE64_STANDARD.decode(cur)?;
    Ok(Some(serde_json::from_slice::<CursorInner>(&raw)?))
}

fn is_balance_client_error(err: &anyhow::Error) -> bool {
    if err
        .root_cause()
        .downcast_ref::<midnight_privacy::PrivacyAddressError>()
        .is_some()
    {
        return true;
    }
    if err.root_cause().downcast_ref::<hex::FromHexError>().is_some() {
        return true;
    }

    let message = err.to_string();
    message.contains("nf_key")
        || message.contains("vfk")
        || message.contains("Invalid privacy address")
        || message.contains("Expected 32-byte hex")
}

async fn get_tx(Path(tx_hash): Path<String>, State(state): State<AppState>) -> impl IntoResponse {
    match crate::db::get_tx(&state.db, &tx_hash).await {
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
    match crate::db::list_txs(&state.db, limit, offset).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============== FVK Registry Management ==============

/// Request body for adding a new FVK
#[derive(Debug, Deserialize)]
pub struct AddFvkRequest {
    /// The FVK as hex string (32 bytes = 64 hex chars)
    pub fvk: String,
    /// Optional: expected fvk_commitment (hex) - if provided, we verify it matches
    #[serde(default)]
    pub fvk_commitment: Option<String>,
    /// The shielded address associated with this FVK (optional)
    #[serde(default)]
    pub shielded_address: Option<String>,
}

/// Response for FVK operations
#[derive(Debug, Serialize)]
pub struct FvkResponse {
    pub fvk_commitment: String,
    pub fvk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
}

/// List all FVKs in the registry
async fn list_fvks(State(state): State<AppState>) -> impl IntoResponse {
    let fvks: Vec<FvkResponse> = state
        .vfk_registry
        .entries()
        .into_iter()
        .map(|(commitment, fvk, addr)| FvkResponse {
            fvk_commitment: commitment,
            fvk: hex::encode(fvk),
            shielded_address: addr,
        })
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "count": fvks.len(),
            "fvks": fvks
        })),
    )
}

/// Add a new FVK to the registry
async fn add_fvk(
    State(state): State<AppState>,
    Json(req): Json<AddFvkRequest>,
) -> impl IntoResponse {
    // Parse the FVK
    let fvk = match viewer::parse_fvk_hex(&req.fvk) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid FVK: {}", e)})),
            )
                .into_response();
        }
    };

    // Compute the commitment from the FVK
    let fvk_obj = midnight_privacy::FullViewingKey(fvk);
    let commitment = midnight_privacy::viewing::fvk_commitment(&fvk_obj);
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
                    "message": "The provided fvk_commitment does not match the commitment computed from the FVK"
                })),
            )
                .into_response();
        }
    }

    // Check if this FVK already exists in the registry
    if state.vfk_registry.get_fvk(&commitment_hex).is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "FVK already exists",
                "fvk_commitment": commitment_hex,
                "message": "A FVK with this commitment is already registered"
            })),
        )
            .into_response();
    }

    // Add to registry (DashMap - no lock needed)
    state.vfk_registry.add(fvk, req.shielded_address.clone());

    // Persist to database
    if let Err(e) = state.vfk_registry.save_to_db(&state.db).await {
        tracing::warn!("Failed to persist FVK to database: {}", e);
    }

    tracing::info!("Added FVK with commitment {}", &commitment_hex[..16]);

    let db = state.db.clone();
    let vfk_registry = state.vfk_registry.clone();
    tokio::spawn(async move {
        if let Err(e) =
            crate::background_sync::backfill_privacy_fields(&db, &vfk_registry).await
        {
            tracing::warn!("VFK backfill failed: {}", e);
        }
    });

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "success": true,
            "fvk_commitment": commitment_hex,
            "message": "FVK added and verified successfully"
        })),
    )
        .into_response()
}

/// Delete a FVK from the registry
async fn delete_fvk(
    Path(fvk_commitment): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    use crate::index_db as idx;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    // Check if it exists and remove (DashMap - no lock needed)
    if !state.vfk_registry.remove(&fvk_commitment) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "FVK not found"})),
        )
            .into_response();
    }

    // Remove from database
    if let Err(e) = idx::fvk_registry::Entity::delete_many()
        .filter(idx::fvk_registry::Column::FvkCommitment.eq(&fvk_commitment))
        .exec(&state.db)
        .await
    {
        tracing::warn!("Failed to delete FVK from database: {}", e);
    }

    tracing::info!(
        "Deleted FVK with commitment {}",
        &fvk_commitment[..16.min(fvk_commitment.len())]
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "message": "FVK deleted successfully"
        })),
    )
        .into_response()
}
