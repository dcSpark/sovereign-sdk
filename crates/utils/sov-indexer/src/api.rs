use crate::balance;
use crate::db;
use crate::db::{
    get_tx_god, list_transactions, list_transactions_god, list_wallet_transactions,
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
    let swagger_ui =
        Router::from(SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()))
            .layer(middleware::from_fn(swagger_ui_redirect));

    Router::new()
        .route("/wallets/:address", post(list_wallet_txs))
        .route("/wallets/:address/balance", post(wallet_balance))
        // New transaction endpoints with privacy modes
        .route("/transactions/:tx_hash", get(get_transaction))
        .route("/transactions/:tx_hash/god", get(get_transaction_god))
        .route("/transactions", get(get_transactions))
        .route("/transactions/god", get(get_transactions_god))
        .route("/transactions/wallet/:wallet", get(get_wallet_transactions))
        .route(
            "/transactions/wallet/:wallet/god",
            get(get_wallet_transactions_god),
        )
        .route("/health", get(health))
        // FVK registry management endpoints
        .route("/fvks", get(list_fvks).post(add_fvk))
        .route("/fvks/:fvk_commitment", delete(delete_fvk))
        // Prefunded wallets management (used by MCP to claim pre-funded wallets)
        .route("/prefunded-wallets/import", post(import_prefunded_wallets))
        .route("/prefunded-wallets/claim", post(claim_prefunded_wallet))
        // Frozen accounts management endpoints
        .route("/frozen", get(list_frozen).post(record_freeze))
        .route("/frozen/:privacy_address", get(get_frozen_status))
        .route("/frozen/:privacy_address/history", get(get_freeze_history))
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
            (status, Json(serde_json::json!({"error": e.to_string()}))).into_response()
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
    if err
        .root_cause()
        .downcast_ref::<hex::FromHexError>()
        .is_some()
    {
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

#[utoipa::path(
    get,
    path = "/transactions/{tx_hash}/god",
    params(
        ("tx_hash" = String, Path, description = "Transaction hash")
    ),
    responses(
        (status = 200, description = "Transaction details (god mode)", body = InvolvementItem),
        (status = 404, description = "Transaction not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "transactions"
)]
async fn get_transaction_god(
    Path(tx_hash): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match get_tx_god(&state.db, &tx_hash).await {
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
    /// The shielded address associated with this FVK (optional, bech32m privpool1...)
    #[serde(default)]
    pub shielded_address: Option<String>,
    /// The public wallet address associated with this FVK (optional, sov1...)
    #[serde(default)]
    pub wallet_address: Option<String>,
}

/// Response for FVK operations
#[derive(Debug, Serialize, ToSchema)]
pub struct FvkResponse {
    pub fvk_commitment: String,
    pub fvk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
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
        .map(
            |(commitment, fvk, shielded_addr, wallet_addr)| FvkResponse {
                fvk_commitment: commitment,
                fvk: hex::encode(fvk),
                shielded_address: shielded_addr,
                wallet_address: wallet_addr,
            },
        )
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
    state.vfk_registry.add(
        fvk,
        req.shielded_address.clone(),
        req.wallet_address.clone(),
    );

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

// ============================================================================
// Prefunded Wallets Endpoints
// ============================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct PrefundedWalletImportItem {
    /// Public wallet address (sov1...)
    pub wallet_address: String,
    /// Privacy address (bech32m privpool1...)
    pub privacy_address: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ImportPrefundedWalletsRequest {
    pub wallets: Vec<PrefundedWalletImportItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportPrefundedWalletsResponse {
    pub processed: usize,
    pub inserted: usize,
    pub ignored: usize,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ClaimPrefundedWalletRequest {
    /// Optional identifier for who is claiming this wallet (e.g. MCP session id)
    #[serde(default)]
    pub claimed_by: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClaimPrefundedWalletResponse {
    pub wallet_address: String,
    pub privacy_address: String,
}

/// Import prefunded wallets into the index DB.
///
/// This endpoint is idempotent: existing rows are not overwritten (especially `used`).
#[utoipa::path(
    post,
    path = "/prefunded-wallets/import",
    request_body = ImportPrefundedWalletsRequest,
    responses(
        (status = 200, description = "Import summary", body = ImportPrefundedWalletsResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "prefunded"
)]
async fn import_prefunded_wallets(
    State(state): State<AppState>,
    Json(req): Json<ImportPrefundedWalletsRequest>,
) -> impl IntoResponse {
    use crate::index_db as idx;
    use sea_orm::{ActiveValue, ConnectionTrait, DatabaseBackend, EntityTrait};
    use std::collections::HashSet;

    const MAX_IMPORT_ITEMS: usize = 50_000;
    if req.wallets.len() > MAX_IMPORT_ITEMS {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Too many items: max {MAX_IMPORT_ITEMS}")
            })),
        )
            .into_response();
    }

    let wallets = req.wallets;
    let mut unique = Vec::new();
    let mut seen = HashSet::<String>::new();
    for item in wallets {
        let wallet_address = item.wallet_address.trim().to_string();
        let privacy_address = item.privacy_address.trim().to_string();
        if wallet_address.is_empty() || privacy_address.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "wallet_address and privacy_address are required"}),
                ),
            )
                .into_response();
        }
        if seen.insert(wallet_address.clone()) {
            unique.push((wallet_address, privacy_address));
        }
    }

    if unique.is_empty() {
        return (
            StatusCode::OK,
            Json(ImportPrefundedWalletsResponse {
                processed: 0,
                inserted: 0,
                ignored: 0,
            }),
        )
            .into_response();
    }

    let processed = unique.len();

    // Chunk inserts to avoid DB bind parameter limits:
    // - SQLite commonly defaults to 999 variables
    // - Postgres has a much higher limit, but very large batches can still hit it
    let backend = state.db.get_database_backend();
    let params_per_row = 4usize; // wallet_address, privacy_address, used, created_at
    let max_params = match backend {
        DatabaseBackend::Sqlite => 900usize,      // keep below 999
        DatabaseBackend::Postgres => 60_000usize, // keep below 65535
        DatabaseBackend::MySql => 60_000usize,
    };
    let mut batch_size = max_params / params_per_row;
    batch_size = batch_size.max(1).min(10_000);

    let mut inserted_total: usize = 0;
    for chunk in unique.chunks(batch_size) {
        let now = chrono::Utc::now();
        let models = chunk
            .iter()
            .cloned()
            .map(|(wallet_address, privacy_address)| {
                idx::prefunded_wallets::ActiveModel {
                    wallet_address: sea_orm::Set(wallet_address),
                    privacy_address: sea_orm::Set(privacy_address),
                    used: sea_orm::Set(false),
                    created_at: sea_orm::Set(now),
                    // Omit claimed fields so they default to NULL without consuming bind params.
                    claimed_at: ActiveValue::NotSet,
                    claimed_by: ActiveValue::NotSet,
                }
            });

        let res = idx::prefunded_wallets::Entity::insert_many(models)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(
                    idx::prefunded_wallets::Column::WalletAddress,
                )
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(&state.db)
            .await;

        match res {
            Ok(rows) => inserted_total = inserted_total.saturating_add(rows as usize),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to import prefunded wallets: {e}")})),
                )
                    .into_response();
            }
        }
    }

    let inserted = inserted_total;
    let ignored = processed.saturating_sub(inserted);

    (
        StatusCode::OK,
        Json(ImportPrefundedWalletsResponse {
            processed,
            inserted,
            ignored,
        }),
    )
        .into_response()
}

/// Claim one available prefunded wallet (marks it as used).
#[utoipa::path(
    post,
    path = "/prefunded-wallets/claim",
    request_body = ClaimPrefundedWalletRequest,
    responses(
        (status = 200, description = "Claimed wallet", body = ClaimPrefundedWalletResponse),
        (status = 404, description = "No wallet available", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "prefunded"
)]
async fn claim_prefunded_wallet(
    State(state): State<AppState>,
    Json(req): Json<ClaimPrefundedWalletRequest>,
) -> impl IntoResponse {
    use crate::index_db as idx;
    use sea_orm::sea_query::Expr;
    use sea_orm::{
        ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter, QueryOrder,
        Statement, TransactionTrait,
    };

    let claimed_by = req
        .claimed_by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let now = chrono::Utc::now();
    let backend = state.db.get_database_backend();

    // Postgres: single-statement claim using SKIP LOCKED to avoid lock waits under contention.
    if backend == DatabaseBackend::Postgres {
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"
WITH candidate AS (
    SELECT wallet_address
    FROM prefunded_wallets
    WHERE used = false
    ORDER BY created_at ASC
    FOR UPDATE SKIP LOCKED
    LIMIT 1
)
UPDATE prefunded_wallets p
SET used = true,
    claimed_at = $1,
    claimed_by = $2
FROM candidate
WHERE p.wallet_address = candidate.wallet_address
RETURNING p.wallet_address, p.privacy_address
            "#,
            [now.into(), claimed_by.clone().into()],
        );

        let row = match state.db.query_one(stmt).await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to claim wallet: {e}")})),
                )
                    .into_response();
            }
        };

        let Some(row) = row else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "No prefunded wallets available"})),
            )
                .into_response();
        };

        let wallet_address: String = match row.try_get_by("wallet_address") {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to decode claimed wallet: {e}")})),
                )
                    .into_response();
            }
        };
        let privacy_address: String = match row.try_get_by("privacy_address") {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to decode claimed wallet: {e}")})),
                )
                    .into_response();
            }
        };

        return (
            StatusCode::OK,
            Json(ClaimPrefundedWalletResponse {
                wallet_address,
                privacy_address,
            }),
        )
            .into_response();
    }

    // Other backends (SQLite dev): best-effort claim with short transactions and retries.
    for attempt in 0..10 {
        let txn = match state.db.begin().await {
            Ok(txn) => txn,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to start transaction: {e}")})),
                )
                    .into_response();
            }
        };

        let candidate = match idx::prefunded_wallets::Entity::find()
            .filter(idx::prefunded_wallets::Column::Used.eq(false))
            .order_by_asc(idx::prefunded_wallets::Column::CreatedAt)
            .one(&txn)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                let _ = txn.rollback().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to query available wallets: {e}")})),
                )
                    .into_response();
            }
        };

        let Some(candidate) = candidate else {
            let _ = txn.rollback().await;
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "No prefunded wallets available"})),
            )
                .into_response();
        };

        let wallet_address = candidate.wallet_address.clone();
        let privacy_address = candidate.privacy_address.clone();

        let update = idx::prefunded_wallets::Entity::update_many()
            .filter(idx::prefunded_wallets::Column::WalletAddress.eq(&wallet_address))
            .filter(idx::prefunded_wallets::Column::Used.eq(false))
            .col_expr(idx::prefunded_wallets::Column::Used, Expr::value(true))
            .col_expr(idx::prefunded_wallets::Column::ClaimedAt, Expr::value(now))
            .col_expr(
                idx::prefunded_wallets::Column::ClaimedBy,
                Expr::value(claimed_by.clone()),
            )
            .exec(&txn)
            .await;

        match update {
            Ok(res) if res.rows_affected == 1 => {
                if let Err(e) = txn.commit().await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": format!("Failed to commit claim: {e}")})),
                    )
                        .into_response();
                }

                return (
                    StatusCode::OK,
                    Json(ClaimPrefundedWalletResponse {
                        wallet_address,
                        privacy_address,
                    }),
                )
                    .into_response();
            }
            Ok(_) => {
                // Lost the race: another concurrent claim updated the row first. Retry.
                let _ = txn.rollback().await;
                continue;
            }
            Err(e) => {
                let msg = e.to_string().to_ascii_lowercase();
                let retryable = backend == DatabaseBackend::Sqlite
                    && (msg.contains("database is locked") || msg.contains("sqlite_busy"));
                let _ = txn.rollback().await;
                if retryable && attempt < 9 {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    continue;
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to claim wallet: {e}")})),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "Failed to claim prefunded wallet after retries"})),
    )
        .into_response()
}

// ============================================================================
// Frozen Accounts Endpoints
// ============================================================================

/// Request body for recording a freeze/unfreeze event
#[derive(Debug, Deserialize, ToSchema)]
pub struct RecordFreezeRequest {
    /// Privacy address (bech32m privpool1...)
    pub privacy_address: String,
    /// Public wallet address (sov1...) if known
    #[serde(default)]
    pub wallet_address: Option<String>,
    /// Reason for freeze/unfreeze
    #[serde(default)]
    pub reason: Option<String>,
    /// Whether this is a freeze (true) or unfreeze (false) event
    pub is_frozen: bool,
    /// Transaction hash that performed this action
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Who initiated this action (admin address)
    #[serde(default)]
    pub initiated_by: Option<String>,
}

/// Response for recording a freeze event
#[derive(Debug, Serialize, ToSchema)]
pub struct RecordFreezeResponse {
    pub success: bool,
    pub id: i64,
    pub message: String,
}

/// Frozen account status for API responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiFrozenAccountStatus {
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
    pub updated_at: String,
}

impl From<db::FrozenAccountStatus> for ApiFrozenAccountStatus {
    fn from(s: db::FrozenAccountStatus) -> Self {
        Self {
            privacy_address: s.privacy_address,
            wallet_address: s.wallet_address,
            is_frozen: s.is_frozen,
            reason: s.reason,
            tx_hash: s.tx_hash,
            initiated_by: s.initiated_by,
            updated_at: s.updated_at.to_rfc3339(),
        }
    }
}

/// Freeze event for API responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiFreezeEvent {
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
    pub created_at: String,
}

impl From<db::FreezeEvent> for ApiFreezeEvent {
    fn from(e: db::FreezeEvent) -> Self {
        Self {
            id: e.id,
            privacy_address: e.privacy_address,
            wallet_address: e.wallet_address,
            reason: e.reason,
            is_frozen: e.is_frozen,
            tx_hash: e.tx_hash,
            initiated_by: e.initiated_by,
            created_at: e.created_at.to_rfc3339(),
        }
    }
}

/// Response for listing frozen accounts
#[derive(Debug, Serialize, ToSchema)]
pub struct FrozenListResponse {
    pub count: usize,
    pub frozen_accounts: Vec<ApiFrozenAccountStatus>,
}

/// Response for freeze history
#[derive(Debug, Serialize, ToSchema)]
pub struct FreezeHistoryResponse {
    pub privacy_address: String,
    pub events: Vec<ApiFreezeEvent>,
}

/// List all currently frozen accounts
#[utoipa::path(
    get,
    path = "/frozen",
    responses(
        (status = 200, description = "List frozen accounts", body = FrozenListResponse)
    ),
    tag = "frozen"
)]
async fn list_frozen(State(state): State<AppState>) -> impl IntoResponse {
    match db::list_frozen_accounts(&state.db, Some(1000)).await {
        Ok(accounts) => {
            let api_accounts: Vec<ApiFrozenAccountStatus> =
                accounts.into_iter().map(|a| a.into()).collect();
            (
                StatusCode::OK,
                Json(FrozenListResponse {
                    count: api_accounts.len(),
                    frozen_accounts: api_accounts,
                }),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to list frozen accounts: {}", e),
            }),
        )
            .into_response(),
    }
}

/// Record a freeze or unfreeze event
#[utoipa::path(
    post,
    path = "/frozen",
    request_body = RecordFreezeRequest,
    responses(
        (status = 201, description = "Freeze event recorded", body = RecordFreezeResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "frozen"
)]
async fn record_freeze(
    State(state): State<AppState>,
    Json(req): Json<RecordFreezeRequest>,
) -> impl IntoResponse {
    // Validate privacy address format
    if !req.privacy_address.starts_with("privpool1") {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid privacy_address format. Expected bech32m privpool1...".to_string(),
            }),
        )
            .into_response();
    }

    match db::record_freeze_event(
        &state.db,
        &req.privacy_address,
        req.wallet_address.as_deref(),
        req.reason.as_deref(),
        req.is_frozen,
        req.tx_hash.as_deref(),
        req.initiated_by.as_deref(),
    )
    .await
    {
        Ok(id) => {
            let action = if req.is_frozen { "frozen" } else { "unfrozen" };
            tracing::info!(
                "Recorded {} event for {} (id={})",
                action,
                &req.privacy_address[..20.min(req.privacy_address.len())],
                id
            );
            (
                StatusCode::CREATED,
                Json(RecordFreezeResponse {
                    success: true,
                    id,
                    message: format!("Account {} successfully", action),
                }),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to record freeze event: {}", e),
            }),
        )
            .into_response(),
    }
}

/// Get current frozen status for a privacy address
#[utoipa::path(
    get,
    path = "/frozen/{privacy_address}",
    params(
        ("privacy_address" = String, Path, description = "Privacy address (bech32m)")
    ),
    responses(
        (status = 200, description = "Frozen status", body = ApiFrozenAccountStatus),
        (status = 404, description = "No freeze records found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "frozen"
)]
async fn get_frozen_status(
    State(state): State<AppState>,
    Path(privacy_address): Path<String>,
) -> impl IntoResponse {
    match db::get_frozen_status(&state.db, &privacy_address).await {
        Ok(Some(status)) => {
            let api_status: ApiFrozenAccountStatus = status.into();
            (StatusCode::OK, Json(api_status)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "No freeze records found for this address".to_string(),
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to get frozen status: {}", e),
            }),
        )
            .into_response(),
    }
}

/// Get freeze/unfreeze history for a privacy address
#[utoipa::path(
    get,
    path = "/frozen/{privacy_address}/history",
    params(
        ("privacy_address" = String, Path, description = "Privacy address (bech32m)")
    ),
    responses(
        (status = 200, description = "Freeze history", body = FreezeHistoryResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    tag = "frozen"
)]
async fn get_freeze_history(
    State(state): State<AppState>,
    Path(privacy_address): Path<String>,
) -> impl IntoResponse {
    match db::get_freeze_history(&state.db, &privacy_address).await {
        Ok(events) => {
            let api_events: Vec<ApiFreezeEvent> = events.into_iter().map(|e| e.into()).collect();
            (
                StatusCode::OK,
                Json(FreezeHistoryResponse {
                    privacy_address,
                    events: api_events,
                }),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to get freeze history: {}", e),
            }),
        )
            .into_response(),
    }
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
        get_transaction_god,
        get_transactions,
        get_transactions_god,
        get_wallet_transactions,
        get_wallet_transactions_god,
        health,
        list_fvks,
        add_fvk,
        delete_fvk,
        import_prefunded_wallets,
        claim_prefunded_wallet,
        list_frozen,
        record_freeze,
        get_frozen_status,
        get_freeze_history
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
        HealthResponse,
        PrefundedWalletImportItem,
        ImportPrefundedWalletsRequest,
        ImportPrefundedWalletsResponse,
        ClaimPrefundedWalletRequest,
        ClaimPrefundedWalletResponse,
        RecordFreezeRequest,
        RecordFreezeResponse,
        FrozenListResponse,
        FreezeHistoryResponse,
        ApiFrozenAccountStatus,
        ApiFreezeEvent
    )),
    tags(
        (name = "wallets", description = "Wallet-related endpoints"),
        (name = "transactions", description = "Transaction endpoints with privacy modes"),
        (name = "fvks", description = "FVK registry management"),
        (name = "prefunded", description = "Prefunded wallet assignment"),
        (name = "frozen", description = "Frozen accounts management"),
        (name = "health", description = "Service health checks")
    )
)]
struct ApiDoc;
