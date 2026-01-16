use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use k256::ecdsa::signature::hazmat::PrehashSigner;
use k256::ecdsa::{Signature, SigningKey};
use midnight_privacy::{fvk_commitment, poseidon2_hash, FullViewingKey, Hash32};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use tracing::info;

mod store;
pub use store::FvkStore;

const DEFAULT_SEED_MAX_LEN: usize = 1024;

#[derive(Clone)]
pub struct AppState {
    issuer: Arc<FvkIssuer>,
    store: Arc<FvkStore>,
}

impl AppState {
    pub fn new(issuer: FvkIssuer, store: FvkStore) -> Self {
        Self {
            issuer: Arc::new(issuer),
            store: Arc::new(store),
        }
    }
}

pub struct FvkIssuer {
    signing_key: SigningKey,
    signing_public_key_hex: String,
    root_fvk_seed: Hash32,
    seed_max_len: usize,
}

impl FvkIssuer {
    pub fn new(signing_key: SigningKey, root_fvk_seed: Hash32) -> anyhow::Result<Self> {
        let pub_key = signing_key.verifying_key().to_encoded_point(true);
        let pub_key_bytes: [u8; 33] = pub_key
            .as_bytes()
            .try_into()
            .map_err(|_| anyhow!("unexpected signer public key size"))?;

        Ok(Self {
            signing_key,
            signing_public_key_hex: hex::encode(pub_key_bytes),
            root_fvk_seed,
            seed_max_len: DEFAULT_SEED_MAX_LEN,
        })
    }

    pub fn with_seed_max_len(mut self, seed_max_len: usize) -> Self {
        self.seed_max_len = seed_max_len;
        self
    }

    pub fn signing_public_key_hex(&self) -> &str {
        &self.signing_public_key_hex
    }

    pub fn issue(&self, seed: &[u8]) -> anyhow::Result<IssuedFvk> {
        anyhow::ensure!(
            seed.len() <= self.seed_max_len,
            "seed too large (max {} bytes)",
            self.seed_max_len
        );

        let fvk_bytes = poseidon2_hash(b"FVK_DERIVE_V1", &[&self.root_fvk_seed, seed]);
        let fvk = FullViewingKey(fvk_bytes);
        let commitment = fvk_commitment(&fvk);

        let signature_bytes = sign_keccak256_prehash(&self.signing_key, &commitment)
            .context("failed to sign fvk_commitment")?;

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
}

#[derive(Debug, Serialize)]
pub struct IssueFvkResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    pub seed_hex: String,
    pub seed_kind: &'static str,
    pub fvk: String,
    pub fvk_commitment: String,
    pub signature: String,
    pub signer_public_key: String,
    pub signature_scheme: &'static str,
    pub fvk_commitment_scheme: &'static str,
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
pub struct ErrorResponse {
    pub error: String,
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
        .route("/v1/fvk", post(issue_fvk))
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
        signature_scheme: "secp256k1-ecdsa-keccak256",
        fvk_commitment_scheme: r#"Poseidon2 H("FVK_COMMIT_V1" || fvk)"#,
        issued_count,
        next_index,
    }))
}

pub async fn issue_fvk(
    State(state): State<AppState>,
    Json(req): Json<IssueFvkRequest>,
) -> Result<Json<IssueFvkResponse>, ApiError> {
    let (seed_bytes, seed_hex, seed_kind, index_value) = resolve_seed_bytes(&*state.store, req)
        .await
        .map_err(ApiError::bad_request)?;

    let issued_at_ms = now_ms().map_err(ApiError::internal)?;

    let issued = state
        .issuer
        .issue(&seed_bytes)
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
        )
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(IssueFvkResponse {
        index: index_value,
        seed_hex,
        seed_kind,
        fvk: hex::encode(issued.fvk),
        fvk_commitment: hex::encode(issued.fvk_commitment),
        signature: hex::encode(issued.signature),
        signer_public_key: issued.signer_public_key_hex,
        signature_scheme: "secp256k1-ecdsa-keccak256",
        fvk_commitment_scheme: r#"Poseidon2 H("FVK_COMMIT_V1" || fvk)"#,
    }))
}

async fn resolve_seed_bytes(
    store: &FvkStore,
    req: IssueFvkRequest,
) -> Result<(Vec<u8>, String, &'static str, Option<u64>), String> {
    match (req.seed, req.seed_hex) {
        (Some(_), Some(_)) => Err("provide only one of seed or seed_hex".to_string()),
        (Some(seed), None) => {
            let bytes = seed.into_bytes();
            Ok((bytes.clone(), hex::encode(bytes), "user", None))
        }
        (None, Some(seed_hex)) => {
            let bytes = parse_hex_bytes(&seed_hex).map_err(|e| e.to_string())?;
            Ok((bytes.clone(), hex::encode(bytes), "user", None))
        }
        (None, None) => {
            let idx = store
                .allocate_index()
                .await
                .map_err(|e| format!("failed to allocate index: {e}"))?;
            let seed = idx.to_le_bytes().to_vec();
            let seed_hex = hex::encode(&seed);
            Ok((seed, seed_hex, "index", Some(idx)))
        }
    }
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, anyhow::Error> {
    let hex_str = s.trim();
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(hex_str).context("invalid hex")
}

fn sign_keccak256_prehash(
    signing_key: &SigningKey,
    msg: &[u8; 32],
) -> Result<[u8; 64], anyhow::Error> {
    let digest: [u8; 32] = Keccak256::digest(msg).into();
    let sig: Signature = signing_key
        .sign_prehash(&digest)
        .map_err(|e| anyhow!("sign_prehash failed: {e}"))?;
    Ok(sig.to_bytes().into())
}

pub fn parse_hex_32(name: &str, value: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = parse_hex_bytes(value).with_context(|| format!("invalid {name}"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("{name} must be 32 bytes"))?;
    Ok(bytes)
}

pub fn generate_signing_key_hex() -> String {
    let signing_key = SigningKey::random(&mut rand::rngs::OsRng);
    hex::encode(signing_key.to_bytes())
}

pub fn generate_root_seed_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn log_startup(bind: SocketAddr) {
    info!("midnight-fvk-service listening on {}", bind);
    info!("GET  {}/health", bind);
    info!("GET  {}/v1/info", bind);
    info!("POST {}/v1/fvk", bind);
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
    use k256::ecdsa::signature::hazmat::PrehashVerifier;

    #[test]
    fn derive_is_deterministic_for_same_seed() {
        let signing_key = SigningKey::random(&mut rand::rngs::OsRng);
        let root_seed = [42u8; 32];
        let issuer = FvkIssuer::new(signing_key, root_seed).unwrap();

        let seed = b"user:alice";
        let a = issuer.issue(seed).unwrap();
        let b = issuer.issue(seed).unwrap();
        assert_eq!(a.fvk, b.fvk);
        assert_eq!(a.fvk_commitment, b.fvk_commitment);
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn signature_verifies_over_commitment() {
        let signing_key = SigningKey::random(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key().clone();

        let root_seed = [7u8; 32];
        let issuer = FvkIssuer::new(signing_key, root_seed).unwrap();

        let issued = issuer.issue(b"seed").unwrap();
        let digest: [u8; 32] = Keccak256::digest(&issued.fvk_commitment).into();
        let sig = Signature::from_slice(&issued.signature).unwrap();
        verifying_key
            .verify_prehash(&digest, &sig)
            .expect("signature should verify");
    }
}
