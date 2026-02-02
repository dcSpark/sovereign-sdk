pub mod config;
use anyhow::Result;
use axum::{
    extract::State,
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    routing::post,
    Router,
};
use config::Config;
use ed25519_dalek::{Signer, SigningKey};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::sync::RwLock;
use tee::common::Engine;
use tee::common::TEEPayload;
use tracing::{error, warn};

#[derive(Debug, Default)]
pub struct MAAPolicyState {
    // Policy id -> Policy data
    pub allowed: HashMap<[u8; 32], String>,
}

pub static MAA_POLICY_STATE: Lazy<RwLock<MAAPolicyState>> =
    Lazy::new(|| RwLock::new(MAAPolicyState::default()));

#[derive(Clone)]
struct AppState {
    signing_key: SigningKey,
    dev_accept_all: bool,
}

pub fn load_policies_from_dir(dir: &String) -> Result<usize> {
    let mut new_map = HashMap::new();

    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let raw = fs::read_to_string(&path)?;

        if serde_json::from_str::<serde_json::Value>(&raw).is_err() {
            warn!("Skipping invalid JSON policy: {:?}", path);
            continue;
        }

        let id: [u8; 32] = Sha256::digest(raw.as_bytes()).into();
        new_map.insert(id, raw);
    }

    let mut st = MAA_POLICY_STATE.write().unwrap();
    st.allowed = new_map;

    Ok(st.allowed.len())
}

fn decode_b64_payload(payload: &TEEPayload) -> Result<Vec<u8>, (StatusCode, &'static str)> {
    if payload.data.len() > 10 * 1024 * 1024 {
        return Err((StatusCode::BAD_REQUEST, "payload too large"));
    }

    let bytes = match tee::common::BASE64_ENGINE.decode(payload.data.trim()) {
        Ok(b) => b,
        Err(_) => return Err((StatusCode::BAD_REQUEST, "invalid base64")),
    };

    Ok(bytes)
}

fn read_policies() -> Vec<([u8; 32], String)> {
    let guard = match MAA_POLICY_STATE.read() {
        Ok(g) => g,
        Err(poisoned) => {
            error!("MAA_POLICY_STATE RwLock poisoned, recovering");
            poisoned.into_inner()
        }
    };

    guard
        .allowed
        .iter()
        .map(|(id, p)| (*id, p.clone()))
        .collect()
}

fn validate_attestation_jwt(
    maa_jwt: &String,
    dev_accept_all: bool,
) -> Result<(), (StatusCode, &'static str)> {
    if dev_accept_all {
        warn!("ORACLE_DEV_ACCEPT_ALL is enabled: skipping policy verification");
        return Ok(());
    }

    let policies = read_policies();
    if policies.is_empty() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "no policies loaded"));
    }

    for (policy_id, policy_data) in policies {
        tracing::info!("Checking MAA policy id: {:02x?}", policy_id);
        match tee::maa::verify(maa_jwt, &policy_data, "midnight-l2") {
            Ok(_) => return Ok(()),
            Err(err) => warn!("Policy {:?} failed: {:?}", policy_id, err),
        }
    }

    Err((StatusCode::FORBIDDEN, "attestation rejected by policy"))
}

async fn validate_batch(State(state): State<AppState>, Json(payload): Json<TEEPayload>) -> impl IntoResponse {
    let bytes = match decode_b64_payload(&payload) {
        Ok(b) => b,
        Err(e) => return e,
    };

    let attestation_payload: sov_modules_api::TEEAttestation = match borsh::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid attestation payload"),
    };

    match attestation_payload.attestation_type {
        sov_modules_api::TEEAttestationType::MAA => {
            // Backward-compatible parsing: accept either a raw JWT string (legacy) or an oracle-signed payload.
            let attestation_jwt: String = match borsh::from_slice::<
                sov_modules_api::TeeOracleSignedMAAAttestationV1,
            >(&attestation_payload.attestation)
            {
                Ok(signed) => signed.attestation_jwt,
                Err(_) => match borsh::from_slice(&attestation_payload.attestation) {
                    Ok(jwt) => jwt,
                    Err(_) => return (StatusCode::BAD_REQUEST, "invalid MAA attestation format"),
                },
            };

            match validate_attestation_jwt(&attestation_jwt, state.dev_accept_all) {
                Ok(()) => (StatusCode::NO_CONTENT, ""),
                Err(e) => e,
            }
        }
        sov_modules_api::TEEAttestationType::RawSevSnp => {
            todo!("RAW SEV-SNP attestation validation not implemented yet");
        }
    }
}

async fn attest_batch(State(state): State<AppState>, Json(payload): Json<TEEPayload>) -> Response {
    let bytes = match decode_b64_payload(&payload) {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    let req: sov_modules_api::OracleAttestRequestV1 = match borsh::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid attest request payload").into_response(),
    };

    if req.statement.domain != sov_modules_api::TEE_ORACLE_STATEMENT_DOMAIN_V1 {
        return (StatusCode::BAD_REQUEST, "invalid statement domain").into_response();
    }

    let jwt_hash: [u8; 32] = Sha256::digest(req.attestation_jwt.as_bytes()).into();
    if req.statement.attestation_jwt_sha256 != jwt_hash {
        return (StatusCode::BAD_REQUEST, "statement JWT hash mismatch").into_response();
    }

    if req.statement.attestation_type != sov_modules_api::TEEAttestationType::MAA {
        return (StatusCode::BAD_REQUEST, "unsupported attestation type").into_response();
    }

    if let Err(e) = validate_attestation_jwt(&req.attestation_jwt, state.dev_accept_all) {
        return e.into_response();
    }

    let message = match borsh::to_vec(&req.statement) {
        Ok(m) => m,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to encode statement").into_response()
        }
    };

    let sig = state.signing_key.sign(&message).to_bytes();
    let resp = sov_modules_api::OracleAttestResponseV1 {
        oracle_pubkey: *state.signing_key.verifying_key().as_bytes(),
        oracle_signature: sig,
    };

    let resp_bytes = match borsh::to_vec(&resp) {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to encode response").into_response()
        }
    };

    (
        StatusCode::OK,
        Json(TEEPayload {
            data: tee::common::BASE64_ENGINE.encode(resp_bytes),
        }),
    )
        .into_response()
}

async fn pubkey(State(state): State<AppState>) -> impl IntoResponse {
    let pk_hex = hex::encode(state.signing_key.verifying_key().as_bytes());
    (StatusCode::OK, pk_hex)
}

async fn root() -> &'static str {
    "Midnight L2 Oracle Service is running."
}

fn parse_signing_key(cfg: &Config) -> Result<SigningKey> {
    let mut key_hex = cfg.oracle_signing_key_hex.clone();
    if key_hex.is_none() {
        if let Some(path) = &cfg.oracle_signing_key_path {
            let raw = fs::read_to_string(path)?;
            key_hex = Some(raw);
        }
    }

    let key_hex = key_hex.expect("Config validated: signing key provided");
    let key_hex = key_hex.trim();
    let key_hex = key_hex.strip_prefix("0x").unwrap_or(key_hex);
    let bytes = hex::decode(key_hex)?;
    let len = bytes.len();
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("ORACLE_SIGNING_KEY_HEX must be 32 bytes (got {len})"))?;

    Ok(SigningKey::from_bytes(&bytes))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = Config::from_env()?;
    let signing_key = parse_signing_key(&cfg)?;

    let count = load_policies_from_dir(&cfg.oracle_policies_dir)?;
    tracing::info!(
        "Loaded {} policies from {:?}",
        count,
        cfg.oracle_policies_dir
    );

    let state = AppState {
        signing_key,
        dev_accept_all: cfg.oracle_dev_accept_all,
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/validate", post(validate_batch))
        .route("/attest", post(attest_batch))
        .route("/pubkey", get(pubkey))
        .with_state(state);
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.oracle_server_bind_address).await?;

    tracing::info!(
        "Server started successfully! Listening on http://{}",
        &cfg.oracle_server_bind_address
    );
    tracing::info!(
        "Validate endpoint: http://{}/validate",
        &cfg.oracle_server_bind_address
    );
    tracing::info!(
        "Attest endpoint: http://{}/attest",
        &cfg.oracle_server_bind_address
    );
    tracing::info!("Pubkey endpoint: http://{}/pubkey", &cfg.oracle_server_bind_address);

    let _ = axum::serve(tcp_listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("\nShutting down");
        })
        .await;

    Ok(())
}
