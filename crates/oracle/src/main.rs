pub mod config;
use anyhow::Result;
use axum::{
    extract::Path,
    extract::Query,
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
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{AnyPool, Row};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::RwLock;
use tee::common::Engine;
use tee::common::TEEPayload;
use tracing::{error, info, warn};

#[derive(Debug, Default)]
pub struct MAAPolicyState {
    // Policy id -> Policy data
    pub allowed: HashMap<[u8; 32], String>,
}

pub static MAA_POLICY_STATE: Lazy<RwLock<MAAPolicyState>> =
    Lazy::new(|| RwLock::new(MAAPolicyState::default()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbType {
    Postgres,
    Sqlite,
}

#[derive(Clone)]
struct AppState {
    signing_key: SigningKey,
    dev_accept_all: bool,
    db_pool: Option<Arc<AnyPool>>,
    db_type: Option<DbType>,
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
    info!(payload_len = payload.data.len(), "POST /validate");

    let bytes = match decode_b64_payload(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!("POST /validate - failed to decode payload");
            return e;
        }
    };

    let attestation_payload: sov_modules_api::TEEAttestation = match borsh::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => {
            warn!("POST /validate - invalid attestation payload");
            return (StatusCode::BAD_REQUEST, "invalid attestation payload");
        }
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
                    Err(_) => {
                        warn!("POST /validate - invalid MAA attestation format");
                        return (StatusCode::BAD_REQUEST, "invalid MAA attestation format");
                    }
                },
            };

            match validate_attestation_jwt(&attestation_jwt, state.dev_accept_all) {
                Ok(()) => {
                    info!("POST /validate - attestation valid");
                    (StatusCode::NO_CONTENT, "")
                }
                Err(e) => {
                    warn!("POST /validate - attestation rejected");
                    e
                }
            }
        }
        sov_modules_api::TEEAttestationType::RawSevSnp => {
            todo!("RAW SEV-SNP attestation validation not implemented yet");
        }
    }
}

async fn attest_batch(State(state): State<AppState>, Json(payload): Json<TEEPayload>) -> Response {
    info!(payload_len = payload.data.len(), "POST /attest");

    let bytes = match decode_b64_payload(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!("POST /attest - failed to decode payload");
            return e.into_response();
        }
    };

    let req: sov_modules_api::OracleAttestRequestV1 = match borsh::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /attest - invalid attest request payload");
            return (StatusCode::BAD_REQUEST, "invalid attest request payload").into_response();
        }
    };

    let batch_index = req.statement.batch_data.batch_index;
    let da_start = req.statement.batch_data.da_start_height;
    let da_end = req.statement.batch_data.da_end_height;

    info!(
        batch_index = batch_index,
        da_start_height = da_start,
        da_end_height = da_end,
        "POST /attest - processing batch"
    );

    if req.statement.domain != sov_modules_api::TEE_ORACLE_STATEMENT_DOMAIN_V1 {
        warn!(batch_index = batch_index, "POST /attest - invalid statement domain");
        return (StatusCode::BAD_REQUEST, "invalid statement domain").into_response();
    }

    let jwt_hash: [u8; 32] = Sha256::digest(req.attestation_jwt.as_bytes()).into();
    if req.statement.attestation_jwt_sha256 != jwt_hash {
        warn!(batch_index = batch_index, "POST /attest - JWT hash mismatch");
        return (StatusCode::BAD_REQUEST, "statement JWT hash mismatch").into_response();
    }

    if req.statement.attestation_type != sov_modules_api::TEEAttestationType::MAA {
        warn!(batch_index = batch_index, "POST /attest - unsupported attestation type");
        return (StatusCode::BAD_REQUEST, "unsupported attestation type").into_response();
    }

    if let Err(e) = validate_attestation_jwt(&req.attestation_jwt, state.dev_accept_all) {
        warn!(batch_index = batch_index, "POST /attest - attestation validation failed");
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

    // Store attestation in database if configured
    if let Some(pool) = &state.db_pool {
        let batch_data = &req.statement.batch_data;
        let attestation_json = serde_json::json!({
            "version": batch_data.version,
            "layer2_chain_id": batch_data.layer2_chain_id,
            "batch_index": batch_data.batch_index,
            "da_start_height": batch_data.da_start_height,
            "da_end_height": batch_data.da_end_height,
            "da_commitment": hex::encode(batch_data.da_commitment),
            "prev_state_root": hex::encode(batch_data.prev_state_root),
            "post_state_root": hex::encode(batch_data.post_state_root),
            "prev_batch_hash": hex::encode(batch_data.prev_batch_hash),
            "batch_hash": hex::encode(batch_data.batch_hash),
            "last_processed_queue_index": batch_data.last_processed_queue_index.to_string(),
            "message_queue_hash": hex::encode(batch_data.message_queue_hash),
            "withdraw_root": hex::encode(batch_data.withdraw_root),
            "attestation_type": format!("{:?}", req.statement.attestation_type),
            "oracle_pubkey": hex::encode(resp.oracle_pubkey),
            "oracle_signature": hex::encode(resp.oracle_signature),
        });

        let batch_idx = batch_data.batch_index as i64;
        let da_start = batch_data.da_start_height as i64;
        let da_end = batch_data.da_end_height as i64;
        let json_str = attestation_json.to_string();

        let query = if state.db_type == Some(DbType::Postgres) {
            r#"
            INSERT INTO tee_attestations (batch_index, da_start_height, da_end_height, attestation_json, created_at)
            VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP)
            ON CONFLICT (batch_index) DO UPDATE SET
                da_start_height = EXCLUDED.da_start_height,
                da_end_height = EXCLUDED.da_end_height,
                attestation_json = EXCLUDED.attestation_json,
                created_at = CURRENT_TIMESTAMP
            "#
        } else {
            r#"
            INSERT INTO tee_attestations (batch_index, da_start_height, da_end_height, attestation_json, created_at)
            VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
            ON CONFLICT (batch_index) DO UPDATE SET
                da_start_height = EXCLUDED.da_start_height,
                da_end_height = EXCLUDED.da_end_height,
                attestation_json = EXCLUDED.attestation_json,
                created_at = CURRENT_TIMESTAMP
            "#
        };

        if let Err(e) = sqlx::query(query)
        .bind(batch_idx)
        .bind(da_start)
        .bind(da_end)
        .bind(&json_str)
        .execute(pool.as_ref())
        .await
        {
            error!(error = ?e, "Failed to store TEE attestation in database");
        } else {
            info!(batch_index = batch_idx, da_start_height = da_start, da_end_height = da_end, "TEE attestation stored in database");
        }
    }

    let resp_bytes = match borsh::to_vec(&resp) {
        Ok(b) => b,
        Err(_) => {
            error!("POST /attest - failed to encode response");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to encode response").into_response()
        }
    };

    info!(
        batch_index = batch_index,
        da_start_height = da_start,
        da_end_height = da_end,
        "POST /attest - attestation signed successfully"
    );

    (
        StatusCode::OK,
        Json(TEEPayload {
            data: tee::common::BASE64_ENGINE.encode(resp_bytes),
        }),
    )
        .into_response()
}

async fn pubkey(State(state): State<AppState>) -> impl IntoResponse {
    info!("GET /pubkey");
    let pk_hex = hex::encode(state.signing_key.verifying_key().as_bytes());
    (StatusCode::OK, pk_hex)
}

async fn root() -> &'static str {
    info!("GET /");
    "Midnight L2 Oracle Service is running."
}

#[derive(Debug, Deserialize)]
struct ListAttestationsParams {
    limit: Option<i64>,
    offset: Option<i64>,
}

/// GET /attestations - List all attestations with pagination
async fn list_attestations(
    State(state): State<AppState>,
    Query(params): Query<ListAttestationsParams>,
) -> Response {
    let limit = params.limit.unwrap_or(20).min(100);
    let offset = params.offset.unwrap_or(0);
    info!(limit = limit, offset = offset, "GET /attestations");

    let Some(pool) = &state.db_pool else {
        warn!("GET /attestations - database not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Database not configured"
            }))
        ).into_response();
    };

    let query = if state.db_type == Some(DbType::Postgres) {
        r#"
        SELECT batch_index, da_start_height, da_end_height, attestation_json, created_at::text as created_at
        FROM tee_attestations
        ORDER BY batch_index DESC
        LIMIT $1 OFFSET $2
        "#
    } else {
        r#"
        SELECT batch_index, da_start_height, da_end_height, attestation_json, CAST(created_at AS TEXT) as created_at
        FROM tee_attestations
        ORDER BY batch_index DESC
        LIMIT ? OFFSET ?
        "#
    };

    let rows = match sqlx::query(query)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.as_ref())
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!(error = ?e, "Failed to query attestations");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Database query failed"
                }))
            ).into_response();
        }
    };

    let attestations: Vec<serde_json::Value> = rows
        .iter()
        .filter_map(|row| {
            let batch_index: i64 = row.try_get("batch_index").ok()?;
            let da_start_height: i64 = row.try_get("da_start_height").ok()?;
            let da_end_height: i64 = row.try_get("da_end_height").ok()?;
            let attestation_json: String = row.try_get("attestation_json").ok()?;
            let created_at: String = row.try_get("created_at").unwrap_or_default();
            let attestation: serde_json::Value = serde_json::from_str(&attestation_json).ok()?;
            Some(serde_json::json!({
                "batch_index": batch_index,
                "da_start_height": da_start_height,
                "da_end_height": da_end_height,
                "created_at": created_at,
                "attestation": attestation,
            }))
        })
        .collect();

    info!(count = attestations.len(), limit = limit, offset = offset, "GET /attestations - returning results");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "attestations": attestations,
            "count": attestations.len(),
            "limit": limit,
            "offset": offset,
        }))
    ).into_response()
}

/// GET /attestations/slot/:slot_id - Get attestation for a specific DA slot height
async fn get_attestation_by_slot(
    State(state): State<AppState>,
    Path(slot_id): Path<i64>,
) -> Response {
    info!(slot_id = slot_id, "GET /attestations/slot/{}", slot_id);

    let Some(pool) = &state.db_pool else {
        warn!(slot_id = slot_id, "GET /attestations/slot - database not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Database not configured"
            }))
        ).into_response();
    };

    let query = if state.db_type == Some(DbType::Postgres) {
        r#"
        SELECT batch_index, da_start_height, da_end_height, attestation_json, created_at::text as created_at
        FROM tee_attestations
        WHERE da_start_height <= $1 AND da_end_height >= $2
        LIMIT 1
        "#
    } else {
        r#"
        SELECT batch_index, da_start_height, da_end_height, attestation_json, CAST(created_at AS TEXT) as created_at
        FROM tee_attestations
        WHERE da_start_height <= ? AND da_end_height >= ?
        LIMIT 1
        "#
    };

    let row = match sqlx::query(query)
    .bind(slot_id)
    .bind(slot_id)
    .fetch_optional(pool.as_ref())
    .await
    {
        Ok(row) => row,
        Err(e) => {
            error!(error = ?e, slot_id = slot_id, "Failed to query attestation by slot");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Database query failed"
                }))
            ).into_response();
        }
    };

    match row {
        Some(row) => {
            let batch_index: i64 = row.try_get("batch_index").unwrap_or(0);
            let da_start_height: i64 = row.try_get("da_start_height").unwrap_or(0);
            let da_end_height: i64 = row.try_get("da_end_height").unwrap_or(0);
            let attestation_json: String = row.try_get("attestation_json").unwrap_or_default();
            let created_at: String = row.try_get("created_at").unwrap_or_default();
            let attestation: serde_json::Value = serde_json::from_str(&attestation_json)
                .unwrap_or(serde_json::Value::Null);

            info!(
                slot_id = slot_id,
                batch_index = batch_index,
                da_start_height = da_start_height,
                da_end_height = da_end_height,
                "GET /attestations/slot - found attestation"
            );

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "found": true,
                    "slot_id": slot_id,
                    "batch_index": batch_index,
                    "da_start_height": da_start_height,
                    "da_end_height": da_end_height,
                    "created_at": created_at,
                    "attestation": attestation,
                }))
            ).into_response()
        }
        None => {
            info!(slot_id = slot_id, "GET /attestations/slot - not found");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "found": false,
                    "slot_id": slot_id,
                    "message": format!("No attestation found for DA height {}", slot_id),
                }))
            ).into_response()
        }
    }
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

fn detect_db_type(connection_string: &str) -> DbType {
    if connection_string.starts_with("postgres") || connection_string.starts_with("postgresql") {
        DbType::Postgres
    } else {
        DbType::Sqlite
    }
}

async fn setup_database(connection_string: &str) -> Result<(AnyPool, DbType)> {
    // Install the any driver for SQLite and PostgreSQL
    sqlx::any::install_default_drivers();

    let db_type = detect_db_type(connection_string);
    let pool = sqlx::AnyPool::connect(connection_string).await?;

    // Create the table if it doesn't exist (SQL syntax works for both SQLite and PostgreSQL)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tee_attestations (
            batch_index BIGINT PRIMARY KEY,
            da_start_height BIGINT NOT NULL,
            da_end_height BIGINT NOT NULL,
            attestation_json TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )
        "#
    )
    .execute(&pool)
    .await?;

    // Create index for querying by DA height range
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_tee_attestations_da_height 
        ON tee_attestations (da_start_height, da_end_height)
        "#
    )
    .execute(&pool)
    .await?;

    info!("TEE attestations database initialized (type: {:?})", db_type);
    Ok((pool, db_type))
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

    // Set up database connection if configured
    let (db_pool, db_type) = if let Some(ref conn_str) = cfg.oracle_db_connection_string {
        match setup_database(conn_str).await {
            Ok((pool, db_type)) => {
                info!("Database connected: {} (type: {:?})", conn_str, db_type);
                (Some(Arc::new(pool)), Some(db_type))
            }
            Err(e) => {
                warn!(error = ?e, "Failed to connect to database, attestations will not be persisted");
                (None, None)
            }
        }
    } else {
        info!("No database configured (ORACLE_DB_CONNECTION_STRING not set), attestations will not be persisted");
        (None, None)
    };

    let state = AppState {
        signing_key,
        dev_accept_all: cfg.oracle_dev_accept_all,
        db_pool,
        db_type,
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/validate", post(validate_batch))
        .route("/attest", post(attest_batch))
        .route("/pubkey", get(pubkey))
        .route("/attestations", get(list_attestations))
        .route("/attestations/slot/{slot_id}", get(get_attestation_by_slot))
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
    tracing::info!("Attestations endpoint: http://{}/attestations", &cfg.oracle_server_bind_address);
    tracing::info!("Attestation by slot endpoint: http://{}/attestations/slot/{{slot_id}}", &cfg.oracle_server_bind_address);

    let _ = axum::serve(tcp_listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("\nShutting down");
        })
        .await;

    Ok(())
}
