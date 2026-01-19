use crate::balance;
use crate::db::{
    list_transactions, list_transactions_god, list_wallet_transactions,
    list_wallet_transactions_god, list_wallet_txs as list_wallet_txs_db, CursorInner,
    InvolvementItem, ListResponse,
};
use crate::viewer::{self, FvkRegistry};
use anyhow::Result;
use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use midnight_privacy::Hash32;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    /// VFK registry using DashMap for lock-free concurrent access
    pub vfk_registry: Arc<FvkRegistry>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ListQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub cursor: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct VfkBody {
    #[serde(default)]
    pub vfk: Option<String>,
}
fn default_limit() -> usize {
    50
}

/// Query parameters for paginated transaction endpoints
#[derive(Debug, Deserialize, ToSchema)]
pub struct TransactionListQuery {
    /// Maximum number of results (default 50, max 200)
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Pagination cursor (base64 encoded)
    pub cursor: Option<String>,
    /// Filter by transaction type: deposit, withdraw, transfer
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FvkListResponse {
    pub count: usize,
    pub fvks: Vec<FvkResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AddFvkResponse {
    pub success: bool,
    pub fvk_commitment: String,
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

pub fn router(state: AppState) -> Router {
    let swagger_ui = Router::from(
        SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()),
    )
    .layer(middleware::from_fn(swagger_ui_redirect));

    Router::new()
        .route("/wallets/:address", post(list_wallet_txs))
        .route("/wallets/:address/balance", post(wallet_balance))
        // New transaction endpoints with privacy modes
        .route("/transactions/:tx_hash", get(get_transaction))
        .route("/transactions", get(get_transactions))
        .route("/transactions/god", get(get_transactions_god))
        .route("/transactions/wallet/:wallet", get(get_wallet_transactions))
        .route("/transactions/wallet/:wallet/god", get(get_wallet_transactions_god))
        .route("/health", get(health))
        // FVK registry management endpoints
        .route("/fvks", get(list_fvks).post(add_fvk))
        .route("/fvks/:fvk_commitment", delete(delete_fvk))
        .merge(swagger_ui)
        .with_state(state)
}

async fn swagger_ui_redirect(req: Request, next: Next) -> Response {
    if req.uri().path() == "/swagger-ui" {
        return Redirect::permanent("/swagger-ui/").into_response();
    }

    next.run(req).await
}

#[utoipa::path(
    post,
    path = "/wallets/{address}",
    params(
        ("address" = String, Path, description = "Wallet or privacy address"),
        ("limit" = Option<usize>, Query, description = "Max results (default 50, max 200)"),
        ("cursor" = Option<String>, Query, description = "Pagination cursor"),
        ("type" = Option<String>, Query, description = "Filter by tx type: deposit, withdraw, transfer")
    ),
    request_body = Option<VfkBody>,
    responses(
        (status = 200, description = "Wallet transactions", body = ListResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "wallets"
)]
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

#[utoipa::path(
    post,
    path = "/wallets/{address}/balance",
    params(
        ("address" = String, Path, description = "Privacy address (privpool1...)")
    ),
    request_body = balance::BalanceRequest,
    responses(
        (status = 200, description = "Wallet balance and unspent notes", body = balance::BalanceResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "wallets"
)]
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

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service healthy", body = HealthResponse)
    ),
    tag = "health"
)]
async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "ok".to_string(),
        }),
    )
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

#[utoipa::path(
    get,
    path = "/transactions/{tx_hash}",
    params(
        ("tx_hash" = String, Path, description = "Transaction hash")
    ),
    responses(
        (status = 200, description = "Transaction details", body = InvolvementItem),
        (status = 404, description = "Transaction not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "transactions"
)]
async fn get_transaction(
    Path(tx_hash): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
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

/// List all transactions (public mode - hides privacy-sensitive fields)
#[utoipa::path(
    get,
    path = "/transactions",
    params(
        ("limit" = Option<usize>, Query, description = "Max results (default 50, max 200)"),
        ("cursor" = Option<String>, Query, description = "Pagination cursor"),
        ("type" = Option<String>, Query, description = "Filter by tx type: deposit, withdraw, transfer")
    ),
    responses(
        (status = 200, description = "Transaction list (public mode)", body = ListResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "transactions"
)]
async fn get_transactions(
    Query(q): Query<TransactionListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let limit = q.limit.min(200);
    let cursor = match decode_cursor(q.cursor) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid cursor: {}", e)})),
            )
                .into_response();
        }
    };

    match list_transactions(&state.db, limit, cursor, q.r#type).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// List all transactions (god mode - shows all fields including decrypted data)
#[utoipa::path(
    get,
    path = "/transactions/god",
    params(
        ("limit" = Option<usize>, Query, description = "Max results (default 50, max 200)"),
        ("cursor" = Option<String>, Query, description = "Pagination cursor"),
        ("type" = Option<String>, Query, description = "Filter by tx type: deposit, withdraw, transfer")
    ),
    responses(
        (status = 200, description = "Transaction list (god mode with decrypted data)", body = ListResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "transactions"
)]
async fn get_transactions_god(
    Query(q): Query<TransactionListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let limit = q.limit.min(200);
    let cursor = match decode_cursor(q.cursor) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid cursor: {}", e)})),
            )
                .into_response();
        }
    };

    match list_transactions_god(&state.db, limit, cursor, q.r#type).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// List transactions for a wallet (public mode - hides privacy-sensitive fields)
#[utoipa::path(
    get,
    path = "/transactions/wallet/{wallet}",
    params(
        ("wallet" = String, Path, description = "Wallet address (L2 or privacy address)"),
        ("limit" = Option<usize>, Query, description = "Max results (default 50, max 200)"),
        ("cursor" = Option<String>, Query, description = "Pagination cursor"),
        ("type" = Option<String>, Query, description = "Filter by tx type: deposit, withdraw, transfer")
    ),
    responses(
        (status = 200, description = "Wallet transactions (public mode)", body = ListResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "transactions"
)]
async fn get_wallet_transactions(
    Path(wallet): Path<String>,
    Query(q): Query<TransactionListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let limit = q.limit.min(200);
    let cursor = match decode_cursor(q.cursor) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid cursor: {}", e)})),
            )
                .into_response();
        }
    };

    match list_wallet_transactions(&state.db, &wallet, limit, cursor, q.r#type).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// List transactions for a wallet (god mode - shows all fields including decrypted data)
#[utoipa::path(
    get,
    path = "/transactions/wallet/{wallet}/god",
    params(
        ("wallet" = String, Path, description = "Wallet address (L2 or privacy address)"),
        ("limit" = Option<usize>, Query, description = "Max results (default 50, max 200)"),
        ("cursor" = Option<String>, Query, description = "Pagination cursor"),
        ("type" = Option<String>, Query, description = "Filter by tx type: deposit, withdraw, transfer")
    ),
    responses(
        (status = 200, description = "Wallet transactions (god mode with decrypted data)", body = ListResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "transactions"
)]
async fn get_wallet_transactions_god(
    Path(wallet): Path<String>,
    Query(q): Query<TransactionListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let limit = q.limit.min(200);
    let cursor = match decode_cursor(q.cursor) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid cursor: {}", e)})),
            )
                .into_response();
        }
    };

    match list_wallet_transactions_god(&state.db, &wallet, limit, cursor, q.r#type).await {
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
#[derive(Debug, Deserialize, ToSchema)]
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
#[derive(Debug, Serialize, ToSchema)]
pub struct FvkResponse {
    pub fvk_commitment: String,
    pub fvk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
}

/// List all FVKs in the registry
#[utoipa::path(
    get,
    path = "/fvks",
    responses(
        (status = 200, description = "List FVKs", body = FvkListResponse)
    ),
    tag = "fvks"
)]
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
        Json(FvkListResponse {
            count: fvks.len(),
            fvks,
        }),
    )
}

/// Add a new FVK to the registry
#[utoipa::path(
    post,
    path = "/fvks",
    request_body = AddFvkRequest,
    responses(
        (status = 201, description = "FVK added", body = AddFvkResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 409, description = "FVK already exists", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "fvks"
)]
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
            crate::background_sync::backfill_privacy_fields(&db, &vfk_registry, None).await
        {
            tracing::warn!("VFK backfill failed: {}", e);
        }
    });

    (
        StatusCode::CREATED,
        Json(AddFvkResponse {
            success: true,
            fvk_commitment: commitment_hex,
            message: "FVK added and verified successfully".to_string(),
        }),
    )
        .into_response()
}

/// Delete a FVK from the registry
#[utoipa::path(
    delete,
    path = "/fvks/{fvk_commitment}",
    params(
        ("fvk_commitment" = String, Path, description = "FVK commitment (hex)")
    ),
    responses(
        (status = 200, description = "FVK deleted", body = SuccessResponse),
        (status = 404, description = "FVK not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "fvks"
)]
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
        Json(SuccessResponse {
            success: true,
            message: "FVK deleted successfully".to_string(),
        }),
    )
        .into_response()
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Sovereign Indexer API",
        version = "0.1.0",
        description = "Indexer API for midnight privacy transactions."
    ),
    paths(
        list_wallet_txs,
        wallet_balance,
        get_transaction,
        get_transactions,
        get_transactions_god,
        get_wallet_transactions,
        get_wallet_transactions_god,
        health,
        list_fvks,
        add_fvk,
        delete_fvk
    ),
    components(schemas(
        ListQuery,
        TransactionListQuery,
        VfkBody,
        balance::BalanceRequest,
        balance::BalanceResponse,
        balance::UnspentNote,
        InvolvementItem,
        ListResponse,
        AddFvkRequest,
        FvkResponse,
        FvkListResponse,
        AddFvkResponse,
        SuccessResponse,
        ErrorResponse,
        HealthResponse
    )),
    tags(
        (name = "wallets", description = "Wallet-related endpoints"),
        (name = "transactions", description = "Transaction endpoints with privacy modes"),
        (name = "fvks", description = "FVK registry management"),
        (name = "health", description = "Service health checks")
    )
)]
struct ApiDoc;
