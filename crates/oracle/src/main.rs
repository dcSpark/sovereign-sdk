pub mod config;
use anyhow::{Context, Result};
use axum::{
    extract::Json, http::StatusCode, response::IntoResponse, routing::get, routing::post, Router,
};
use config::Config;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::{collections::HashSet, sync::RwLock};
use std::{fs, path::Path};
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

async fn validate_batch(Json(payload): Json<TEEPayload>) -> impl IntoResponse {
    if payload.data.len() > 10 * 1024 * 1024 {
        return (StatusCode::BAD_REQUEST, "payload too large");
    }

    let bytes = match tee::common::BASE64_ENGINE.decode(payload.data.trim()) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid base64"),
    };

    let attestation_payload: sov_modules_api::TEEAttestation = match borsh::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid attestation payload"),
    };

    let policies: Vec<([u8; 32], String)> = {
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
    };

    match attestation_payload.attestation_type {
        sov_modules_api::TEEAttestationType::MAA => {
            let attestation_jwt: String = match borsh::from_slice(&attestation_payload.attestation)
            {
                Ok(jwt) => jwt,
                Err(_) => return (StatusCode::BAD_REQUEST, "invalid MAA attestation format"),
            };

            for (policy_id, policy_data) in policies {
                tracing::info!("Checking MAA policy id: {:02x?}", policy_id);
                match tee::maa::verify(&attestation_jwt, &policy_data, "midnight-l2") {
                    Ok(_) => return (StatusCode::NO_CONTENT, ""),
                    Err(err) => warn!("Policy {:?} failed: {:?}", policy_id, err),
                }
            }

            return (StatusCode::FORBIDDEN, "attestation rejected by policy");
        }
        sov_modules_api::TEEAttestationType::RawSevSnp => {
            todo!("RAW SEV-SNP attestation validation not implemented yet");
        }
    }
}

async fn root() -> &'static str {
    "Midnight L2 Oracle Service is running."
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = Config::from_env()?;

    let count = load_policies_from_dir(&cfg.oracle_policies_dir)?;
    tracing::info!(
        "[oracle] Loaded {} policies from {:?}",
        count,
        cfg.oracle_policies_dir
    );

    let app = Router::new()
        .route("/", get(root))
        .route("/validate", post(validate_batch));
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.oracle_server_bind_address).await?;

    tracing::info!(
        "[oracle] Server started successfully! Listening on http://{}",
        &cfg.oracle_server_bind_address
    );
    tracing::info!(
        "[oracle] validate endpoint: http://{}/validate",
        &cfg.oracle_server_bind_address
    );

    let _ = axum::serve(tcp_listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("\n[oracle] Shutting down gracefully...");
        })
        .await;

    Ok(())
}
