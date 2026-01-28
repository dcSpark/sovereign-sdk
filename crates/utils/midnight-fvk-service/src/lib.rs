use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::{Signer, SigningKey};
use midnight_privacy::{fvk_commitment, FullViewingKey, Hash32};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::info;

mod store;
pub use store::FvkStore;

#[derive(Clone)]
pub struct AppState {
    issuer: Arc<FvkIssuer>,
    store: Arc<FvkStore>,
    admin_token: Option<String>,
}

impl AppState {
    pub fn new(issuer: FvkIssuer, store: FvkStore, admin_token: Option<String>) -> Self {
        Self {
            issuer: Arc::new(issuer),
            store: Arc::new(store),
            admin_token,
        }
    }
}

pub struct FvkIssuer {
    signing_key: SigningKey,
    signing_public_key_hex: String,
}

impl FvkIssuer {
    pub fn new(signing_key: SigningKey) -> anyhow::Result<Self> {
        Ok(Self {
            signing_public_key_hex: hex::encode(signing_key.verifying_key().as_bytes()),
            signing_key,
        })
    }

    pub fn signing_public_key_hex(&self) -> &str {
        &self.signing_public_key_hex
    }

    pub fn issue(&self) -> anyhow::Result<IssuedFvk> {
        let mut fvk_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut fvk_bytes);
        let fvk = FullViewingKey(fvk_bytes);
        let commitment = fvk_commitment(&fvk);

        let signature_bytes = sign_ed25519(&self.signing_key, &commitment);

        Ok(IssuedFvk {
            fvk: fvk.0,
            fvk_commitment: commitment,
            signature: signature_bytes,
            signer_public_key_hex: self.signing_public_key_hex.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedFvk {
    pub fvk: Hash32,
    pub fvk_commitment: Hash32,
    pub signature: [u8; 64],
    pub signer_public_key_hex: String,
}

#[derive(Debug, Deserialize)]
pub struct IssueFvkRequest {
    #[serde(default)]
    pub seed: Option<String>,
    #[serde(default)]
    pub seed_hex: Option<String>,
    /// Optional shielded address (bech32m privpool1...) to associate with this FVK
    #[serde(default)]
    pub shielded_address: Option<String>,
    /// Optional public wallet address (sov1...) to associate with this FVK
    #[serde(default)]
    pub wallet_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IssueFvkResponse {
    pub fvk: String,
    pub fvk_commitment: String,
    pub signature: String,
    pub signer_public_key: String,
    pub signature_scheme: &'static str,
    pub fvk_commitment_scheme: &'static str,
    /// The shielded address associated with this FVK (if provided at issuance)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
    /// The public wallet address associated with this FVK (if provided at issuance)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InfoResponse {
    pub signer_public_key: String,
    pub signature_scheme: &'static str,
    pub fvk_commitment_scheme: &'static str,
    pub issued_count: u64,
    pub next_index: u64,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct LookupFvkResponse {
    pub fvk: String,
    pub fvk_commitment: String,
    /// The shielded address associated with this FVK (if stored)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
    /// The public wallet address associated with this FVK (if stored)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Request body for updating the addresses of an existing FVK
#[derive(Debug, Deserialize)]
pub struct UpdateAddressesRequest {
    #[serde(default)]
    pub shielded_address: Option<String>,
    #[serde(default)]
    pub wallet_address: Option<String>,
}

/// Response for listing all FVKs
#[derive(Debug, Serialize)]
pub struct ListFvksResponse {
    pub count: usize,
    pub fvks: Vec<FvkListItem>,
}

/// Individual FVK entry in the list response
#[derive(Debug, Serialize)]
pub struct FvkListItem {
    pub fvk: String,
    pub fvk_commitment: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shielded_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorResponse {
            error: self.message,
        });
        (self.status, body).into_response()
    }
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/info", get(info))
        .route("/v1/fvk", post(issue_fvk).get(list_fvks))
        .route("/v1/fvk/:fvk_commitment", get(lookup_fvk))
        .route(
            "/v1/fvk/:fvk_commitment/address",
            post(update_shielded_address),
        )
        .with_state(state)
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

pub async fn info(State(state): State<AppState>) -> Result<Json<InfoResponse>, ApiError> {
    let issued_count = state
        .store
        .count_issued()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let next_index = state
        .store
        .get_next_index()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(InfoResponse {
        signer_public_key: state.issuer.signing_public_key_hex().to_string(),
        signature_scheme: "ed25519",
        fvk_commitment_scheme: r#"Poseidon2 H("FVK_COMMIT_V1" || fvk)"#,
        issued_count,
        next_index,
    }))
}

pub async fn issue_fvk(
    State(state): State<AppState>,
    Json(req): Json<IssueFvkRequest>,
) -> Result<Json<IssueFvkResponse>, ApiError> {
    if req
        .seed
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .is_some()
        || req
            .seed_hex
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .is_some()
    {
        return Err(ApiError::bad_request(
            "seed/seed_hex are disabled; omit them to receive a fresh FVK",
        ));
    }

    let shielded_address = req
        .shielded_address
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let wallet_address = req
        .wallet_address
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let issued_at_ms = now_ms().map_err(ApiError::internal)?;
    let index_value = state
        .store
        .allocate_index()
        .await
        .map(Some)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let seed_bytes = index_value.expect("set above").to_le_bytes();

    let issued = state
        .issuer
        .issue()
        .map_err(|e| ApiError::internal(e.to_string()))?;

    state
        .store
        .record_issue(
            &issued.fvk_commitment,
            &issued.fvk,
            &seed_bytes,
            issued_at_ms,
            index_value,
            &issued.signature,
            shielded_address,
            wallet_address,
        )
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(IssueFvkResponse {
        fvk: hex::encode(issued.fvk),
        fvk_commitment: hex::encode(issued.fvk_commitment),
        signature: hex::encode(issued.signature),
        signer_public_key: issued.signer_public_key_hex,
        signature_scheme: "ed25519",
        fvk_commitment_scheme: r#"Poseidon2 H("FVK_COMMIT_V1" || fvk)"#,
        shielded_address: shielded_address.map(|s| s.to_string()),
        wallet_address: wallet_address.map(|s| s.to_string()),
    }))
}

pub async fn lookup_fvk(
    State(state): State<AppState>,
    Path(fvk_commitment_hex): Path<String>,
    headers: HeaderMap,
) -> Result<Json<LookupFvkResponse>, ApiError> {
    require_admin_token(&state, &headers)?;
    let fvk_commitment = parse_hex_32("fvk_commitment", &fvk_commitment_hex)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let maybe_fvk = state
        .store
        .get_fvk_by_commitment(&fvk_commitment)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let Some((fvk, shielded_address, wallet_address)) = maybe_fvk else {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "fvk_commitment not found".to_string(),
        });
    };

    Ok(Json(LookupFvkResponse {
        fvk: hex::encode(fvk),
        fvk_commitment: hex::encode(fvk_commitment),
        shielded_address,
        wallet_address,
    }))
}

/// List all issued FVKs (requires admin token)
pub async fn list_fvks(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ListFvksResponse>, ApiError> {
    require_admin_token(&state, &headers)?;

    let all_fvks = state
        .store
        .list_all()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let fvks: Vec<FvkListItem> = all_fvks
        .into_iter()
        .map(
            |(commitment, fvk, shielded_address, wallet_address)| FvkListItem {
                fvk: hex::encode(fvk),
                fvk_commitment: hex::encode(commitment),
                shielded_address,
                wallet_address,
            },
        )
        .collect();

    Ok(Json(ListFvksResponse {
        count: fvks.len(),
        fvks,
    }))
}

/// Update the addresses for an existing FVK (requires admin token)
pub async fn update_shielded_address(
    State(state): State<AppState>,
    Path(fvk_commitment_hex): Path<String>,
    headers: HeaderMap,
    Json(req): Json<UpdateAddressesRequest>,
) -> Result<Json<LookupFvkResponse>, ApiError> {
    require_admin_token(&state, &headers)?;

    let fvk_commitment = parse_hex_32("fvk_commitment", &fvk_commitment_hex)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let shielded_address = req
        .shielded_address
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let wallet_address = req
        .wallet_address
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    if shielded_address.is_none() && wallet_address.is_none() {
        return Err(ApiError::bad_request(
            "At least one of shielded_address or wallet_address must be provided",
        ));
    }

    // Verify the FVK exists
    let maybe_fvk = state
        .store
        .get_fvk_by_commitment(&fvk_commitment)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let Some((fvk, existing_shielded, existing_wallet)) = maybe_fvk else {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "fvk_commitment not found".to_string(),
        });
    };

    // Update the addresses
    state
        .store
        .update_addresses(&fvk_commitment, shielded_address, wallet_address)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    info!(
        "Updated addresses for FVK commitment {}",
        &fvk_commitment_hex[..16.min(fvk_commitment_hex.len())]
    );

    Ok(Json(LookupFvkResponse {
        fvk: hex::encode(fvk),
        fvk_commitment: hex::encode(fvk_commitment),
        shielded_address: shielded_address
            .map(|s| s.to_string())
            .or(existing_shielded),
        wallet_address: wallet_address.map(|s| s.to_string()).or(existing_wallet),
    }))
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, anyhow::Error> {
    let hex_str = s.trim();
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(hex_str).context("invalid hex")
}

fn require_admin_token(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = state
        .admin_token
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "not found".to_string(),
        })?;

    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let got = auth.strip_prefix("Bearer ").unwrap_or("");
    if got == expected {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "unauthorized".to_string(),
        })
    }
}

fn sign_ed25519(signing_key: &SigningKey, msg: &[u8; 32]) -> [u8; 64] {
    signing_key.sign(msg).to_bytes()
}

pub fn parse_hex_32(name: &str, value: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = parse_hex_bytes(value).with_context(|| format!("invalid {name}"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("{name} must be 32 bytes"))?;
    Ok(bytes)
}

pub fn generate_signing_key_hex() -> String {
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    hex::encode(signing_key.to_bytes())
}

pub fn log_startup(bind: SocketAddr) {
    info!("midnight-fvk-service listening on {}", bind);
    info!("GET  {}/health", bind);
    info!("GET  {}/v1/info", bind);
    info!("POST {}/v1/fvk (shielded_address optional)", bind);
    info!(
        "GET  {}/v1/fvk (list all, requires MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN)",
        bind
    );
    info!(
        "GET  {}/v1/fvk/<fvk_commitment> (requires MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN)",
        bind
    );
    info!(
        "POST {}/v1/fvk/<fvk_commitment>/address (update shielded_address, requires MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN)",
        bind
    );
}

fn now_ms() -> Result<i64, String> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_err(|e| format!("clock error: {e}"))?;
    let ms: i128 = i128::from(d.as_secs()) * 1000 + i128::from(d.subsec_millis());
    i64::try_from(ms).map_err(|_| "timestamp overflow".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signature;

    #[test]
    fn issue_generates_fresh_fvk() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let issuer = FvkIssuer::new(signing_key).unwrap();

        let a = issuer.issue().unwrap();
        let b = issuer.issue().unwrap();
        assert_ne!(a.fvk, b.fvk);
        assert_ne!(a.fvk_commitment, b.fvk_commitment);
    }

    #[test]
    fn signature_verifies_over_commitment() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let issuer = FvkIssuer::new(signing_key).unwrap();

        let issued = issuer.issue().unwrap();
        verifying_key
            .verify_strict(
                &issued.fvk_commitment,
                &Signature::from_bytes(&issued.signature),
            )
            .expect("signature should verify");
    }
}
