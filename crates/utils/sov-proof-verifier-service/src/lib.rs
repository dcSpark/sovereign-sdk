//! Off-chain Parallel Proof Verification Service
//!
//! This service receives signed Ligero transactions, verifies them in parallel,
//! and transforms them into non-ZK transactions for the rollup node.

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{prelude::BASE64_STANDARD, Engine};
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey as Ed25519VerifyingKey};
use futures::future::join_all;
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectOptions,
    ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
};
use serde::{Deserialize, Serialize};
use sov_api_spec::types::AcceptTxBody;
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment};
use sov_modules_api::{
    capabilities::UniquenessData,
    configurable_spec::ConfigurableSpec,
    execution_mode::Native,
    gas::UnlimitedGasMeter,
    transaction::{PriorityFeeBips, Transaction, TxDetails, UnsignedTransaction},
    Amount, DispatchCall, Spec,
};
use sov_node_client::NodeClient;
use sov_rollup_interface::{
    crypto::PrivateKey,
    crypto::PublicKey,
    zk::{CodeCommitment, CryptoSpec, Zkvm, ZkvmHost},
};
use std::sync::OnceLock;
use std::{path::Path, sync::Arc};
use tracing::{debug, error, info, warn};

// Import the actual demo-stf Runtime types
use demo_stf::runtime::Runtime as DemoRuntime;
use midnight_privacy::{
    CallMessage as MidnightCallMessage, EncryptedNote, FullViewingKey, Hash32 as MidnightHash32,
    SpendPublic,
};
use sov_address::MultiAddressEvm;
use sov_midnight_da::storable::{
    setup_db as setup_midnight_da_db, worker_verified_transactions, IncomingWorkerTxSaver,
};
use sov_midnight_da::MidnightDaSpec;
use sov_mock_zkvm::MockZkvm;

/// The rollup's Spec type (must match rollup-ligero configuration)
pub type RollupSpec = ConfigurableSpec<MidnightDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;

type RuntimeCall = <DemoRuntime<RollupSpec> as DispatchCall>::Decodable;
type DemoTransaction = Transaction<DemoRuntime<RollupSpec>, RollupSpec>;

/// Configuration for the proof verifier service
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// URL of the rollup node RPC endpoint
    pub node_rpc_url: String,
    /// Private key path for signing non-ZK transactions
    pub signing_key_path: String,
    /// Ligero method ID for value-setter proof verification
    /// If None, it will be computed from the value_validator_rust.wasm program
    pub value_setter_method_id: Option<[u8; 32]>,
    /// Ligero method ID for midnight note_spend_guest proof verification
    /// If None, it will be computed from the note_spend_guest.wasm program
    pub midnight_method_id: Option<[u8; 32]>,
    /// Maximum number of concurrent verification tasks
    pub max_concurrent_verifications: usize,
    /// Chain ID for transaction authentication
    pub chain_id: u64,
    /// Connection string for the shared MockDA database
    pub da_connection_string: String,
    /// If true, do NOT submit to sequencer immediately; queue and wait for an explicit flush.
    /// Useful for benchmarks to remove the worker bottleneck and release all txs at once.
    pub defer_sequencer_submission: bool,
    /// URL of the ligero-http-server prover/verifier service (default: http://localhost:1313)
    pub prover_service_url: Option<String>,
}

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    config: Arc<ServiceConfig>,
    node_client: NodeClient,
    http_client: reqwest::Client,
    rollup_chain_hash: [u8; 32],
    /// Semaphore to limit concurrent verifications
    verification_semaphore: Arc<tokio::sync::Semaphore>,
    /// Local nonce counter (synchronized across all requests)
    nonce_counter: Arc<tokio::sync::Mutex<Option<u64>>>,
    /// Cached signing key (loaded once at startup)
    signing_key: Arc<<<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey>,
    /// Connection to the MockDA database shared with the rollup node
    da_conn: Arc<DatabaseConnection>,
    /// Optional persistence of the full incoming worker tx blob (configurable via rollup_config.toml [da]).
    incoming_worker_tx_saver: IncomingWorkerTxSaver,
    /// Optional pool public key used to authenticate signed viewer commitments (FVK commitments).
    ///
    /// When set via `POOL_FVK_PK`, Transfer/Withdraw transactions must carry a pool signature
    /// over the viewer `fvk_commitment` inside the Ligero proof package args (see
    /// `enforce_pool_signed_viewer_commitment`).
    pool_fvk_pk: Option<Ed25519VerifyingKey>,
}

#[derive(Deserialize)]
struct SchemaResp {
    chain_hash: String,
}

async fn fetch_rollup_chain_hash(node_client: &NodeClient) -> Result<[u8; 32]> {
    let schema: SchemaResp = node_client
        .query_rest_endpoint("/rollup/schema")
        .await
        .context("Failed to fetch /rollup/schema from rollup node")?;
    let chain_hash_hex = schema.chain_hash.trim_start_matches("0x");
    let chain_hash_vec = hex::decode(chain_hash_hex)
        .with_context(|| format!("Invalid chain_hash returned by node: {}", schema.chain_hash))?;
    if chain_hash_vec.len() != 32 {
        return Err(anyhow::anyhow!(
            "chain_hash must be 32 bytes (got {})",
            chain_hash_vec.len()
        ));
    }
    let mut chain_hash = [0u8; 32];
    chain_hash.copy_from_slice(&chain_hash_vec);
    Ok(chain_hash)
}

impl AppState {
    pub async fn new(config: ServiceConfig) -> Result<Self, anyhow::Error> {
        Self::new_with_incoming_worker_tx_saver(config, IncomingWorkerTxSaver::disabled()).await
    }

    pub async fn new_with_incoming_worker_tx_saver(
        mut config: ServiceConfig,
        incoming_worker_tx_saver: IncomingWorkerTxSaver,
    ) -> Result<Self, anyhow::Error> {
        fn parse_ed25519_pubkey_hex(env_name: &str, value: &str) -> Result<Ed25519VerifyingKey> {
            let hex_str = value.trim();
            let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
            let bytes = hex::decode(hex_str)
                .with_context(|| format!("Failed to decode {env_name} as hex"))?;
            let len = bytes.len();
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                anyhow::anyhow!("{env_name} must be a 32-byte ed25519 public key (got {len} bytes)")
            })?;
            Ed25519VerifyingKey::from_bytes(&bytes)
                .with_context(|| format!("{env_name} must be a valid ed25519 public key"))
        }

        // Allow skipping cryptographic verification via env var.
        // If any of these env vars are truthy, set LIGERO_SKIP_VERIFICATION=1 so
        // sov_ligero_adapter::LigeroVerifier returns the public output without verifying.
        fn env_truthy(name: &str) -> bool {
            std::env::var(name)
                .ok()
                .map(|v| {
                    let v = v.to_ascii_lowercase();
                    v == "1" || v == "true" || v == "yes" || v == "on"
                })
                .unwrap_or(false)
        }
        if env_truthy("SOV_PROOF_VERIFIER_SKIP_VERIFY")
            || env_truthy("SKIP_VERIFY")
            || env_truthy("LIGERO_SKIP_VERIFICATION")
        {
            // Ensure the adapter sees this flag
            std::env::set_var("LIGERO_SKIP_VERIFICATION", "1");
            info!(
                "Proof verification skipping is ENABLED (env var set) — returning public outputs without verification"
            );
        }

        let pool_fvk_pk = match std::env::var("POOL_FVK_PK") {
            Ok(v) if !v.trim().is_empty() => {
                let verifying_key = parse_ed25519_pubkey_hex("POOL_FVK_PK", &v)?;
                info!(
                    "POOL_FVK_PK set: enforcing pool-signed viewer commitments (pk={})",
                    hex::encode(verifying_key.as_bytes())
                );
                Some(verifying_key)
            }
            _ => None,
        };

        let max_permits = config.max_concurrent_verifications;
        let node_client = NodeClient::new_unchecked(&config.node_rpc_url);

        let rollup_chain_hash = fetch_rollup_chain_hash(&node_client).await?;
        info!(
            "Using rollup chain hash from /rollup/schema: 0x{}",
            hex::encode(rollup_chain_hash)
        );

        // Load signing key once at startup
        let signing_key = load_private_key(&config.signing_key_path)
            .context("Failed to load signing key at startup")?;

        info!("✓ Loaded signing key from: {}", config.signing_key_path);

        // Compute value-setter method ID if not provided
        if config.value_setter_method_id.is_none() {
            info!("Computing value-setter method ID from value_validator_rust.wasm...");
            match compute_value_setter_method_id() {
                Ok(method_id) => {
                    info!("✓ Value-setter method ID: 0x{}", hex::encode(method_id));
                    config.value_setter_method_id = Some(method_id);
                }
                Err(e) => {
                    error!("Failed to compute value-setter method ID: {}", e);
                    info!("Value-setter endpoint will not be available");
                }
            }
        }

        // Compute midnight method ID if not provided
        if config.midnight_method_id.is_none() {
            info!("Computing midnight method ID from note_spend_guest.wasm...");
            match compute_midnight_method_id() {
                Ok(method_id) => {
                    info!("✓ Midnight method ID: 0x{}", hex::encode(method_id));
                    config.midnight_method_id = Some(method_id);
                }
                Err(e) => {
                    error!("Failed to compute midnight method ID: {}", e);
                    info!("Midnight endpoint will not be available");
                }
            }
        }

        // For SQLite, we need to build SqliteConnectOptions with busy_timeout
        // For other databases, use standard ConnectOptions
        let da_conn_string_redacted = redact_db_connection_string(&config.da_connection_string);
        let da_conn = if config.da_connection_string.starts_with("sqlite:") {
            use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
            use std::str::FromStr;

            // Parse connection string and set busy_timeout
            let sqlite_opts = SqliteConnectOptions::from_str(&config.da_connection_string)
                .with_context(|| {
                    format!(
                        "Failed to parse SQLite connection string: {}",
                        da_conn_string_redacted
                    )
                })?
                .busy_timeout(std::time::Duration::from_millis(30000)); // 30 seconds

            // Create pool with optimized settings for SQLite
            let pool = SqlitePoolOptions::new()
                .max_connections(10) // Conservative for SQLite (single-writer)
                .min_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(30))
                .idle_timeout(Some(std::time::Duration::from_secs(300)))
                .max_lifetime(Some(std::time::Duration::from_secs(1800)))
                .connect_with(sqlite_opts)
                .await
                .with_context(|| {
                    format!(
                        "Failed to connect to SQLite database at {}",
                        da_conn_string_redacted
                    )
                })?;

            info!(
                db = %da_conn_string_redacted,
                "Verifier service connected to SQLite database (max_connections=10, busy_timeout=30s)"
            );

            DatabaseConnection::SqlxSqlitePoolConnection(pool.into())
        } else {
            // PostgreSQL or other databases
            let mut connect_opts = ConnectOptions::new(config.da_connection_string.clone());

            connect_opts
                .max_connections(20)
                .min_connections(1)
                .connect_timeout(std::time::Duration::from_secs(30))
                .acquire_timeout(std::time::Duration::from_secs(30))
                .idle_timeout(std::time::Duration::from_secs(300))
                .max_lifetime(std::time::Duration::from_secs(1800))
                .sqlx_logging(false);

            info!(
                db = %da_conn_string_redacted,
                "Verifier service connecting to PostgreSQL database (max_connections=20)"
            );

            Database::connect(connect_opts).await.with_context(|| {
                format!(
                    "Failed to connect to PostgreSQL database at {}",
                    da_conn_string_redacted
                )
            })?
        };
        setup_midnight_da_db(&da_conn)
            .await
            .context("Failed to initialize MockDA database schema")?;

        info!(
            db = %da_conn_string_redacted,
            backend = ?da_conn.get_database_backend(),
            node_rpc_url = %config.node_rpc_url,
            defer_sequencer_submission = config.defer_sequencer_submission,
            "✓ Connected to MockDA database (busy_timeout applied per-connection). \
             NOTE: The sequencer must be configured to use the SAME database for worker_verified_transactions lookups."
        );

        Ok(Self {
            config: Arc::new(config),
            node_client,
            http_client: reqwest::Client::new(),
            rollup_chain_hash,
            verification_semaphore: Arc::new(tokio::sync::Semaphore::new(max_permits)),
            nonce_counter: Arc::new(tokio::sync::Mutex::new(None)),
            signing_key: Arc::new(signing_key),
            da_conn: Arc::new(da_conn),
            incoming_worker_tx_saver,
            pool_fvk_pk,
        })
    }
}

fn ligero_skip_verify_enabled() -> bool {
    std::env::var("LIGERO_SKIP_VERIFICATION")
        .ok()
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

fn parse_ligero_i64_arg(v: &serde_json::Value, label: &str) -> Result<i64, ServiceError> {
    v.get("i64").and_then(|v| v.as_i64()).ok_or_else(|| {
        ServiceError::ParseError(format!("Expected Ligero i64 argument for {label}"))
    })
}

fn decode_hex_bytes(label: &str, s: &str) -> Result<Vec<u8>, ServiceError> {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).map_err(|e| ServiceError::ParseError(format!("Invalid hex for {label}: {e}")))
}

fn decode_ligero_hash32_arg(
    v: &serde_json::Value,
    label: &str,
) -> Result<MidnightHash32, ServiceError> {
    let obj = v.as_object().ok_or_else(|| {
        ServiceError::ParseError(format!("Expected Ligero arg object for {label}"))
    })?;

    if let Some(b64) = obj.get("bytes_b64").and_then(|v| v.as_str()) {
        let bytes = BASE64_STANDARD.decode(b64).map_err(|e| {
            ServiceError::ParseError(format!("Invalid base64 in {label}.bytes_b64: {e}"))
        })?;
        let len = bytes.len();
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            ServiceError::ParseError(format!(
                "{label}.bytes_b64 must decode to 32 bytes, got {len}",
            ))
        })?;
        return Ok(bytes);
    }

    let hex_str = obj
        .get("hex")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ServiceError::ParseError(format!("Missing {label}.hex")))?;
    let bytes = decode_hex_bytes(&format!("{label}.hex"), hex_str)?;
    let len = bytes.len();
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        ServiceError::ParseError(format!("{label}.hex must be 32 bytes, got {len}"))
    })?;
    Ok(bytes)
}

fn decode_pool_signature_from_ligero_arg(
    v: &serde_json::Value,
    label: &str,
) -> Result<[u8; 64], ServiceError> {
    let obj = v.as_object().ok_or_else(|| {
        ServiceError::ParseError(format!("Expected Ligero arg object for {label}"))
    })?;

    // Accept a few common spellings to match upstream payloads.
    let sig_hex = obj
        .get("pool_sig_hex")
        .or_else(|| obj.get("signature"))
        .or_else(|| obj.get("pool_signature"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ServiceError::SignatureError(format!(
                "Missing pool signature on viewer fvk_commitment arg (expected one of: pool_sig_hex, signature, pool_signature)"
            ))
        })?;

    let bytes = decode_hex_bytes("pool signature", sig_hex)?;
    let len = bytes.len();
    let bytes: [u8; 64] = bytes.try_into().map_err(|_| {
        ServiceError::ParseError(format!(
            "Pool signature must be 64 bytes (128 hex chars), got {len} bytes",
        ))
    })?;
    Ok(bytes)
}

/// Locate the Level-B viewer section and return the index of the first viewer `fvk_commitment` arg.
///
/// This follows the fixed ABI described in `crates/adapters/ligero/reference_circuits/note_spend_guest_v2.rs`.
fn locate_viewer_fvk_commitment_index(
    args: &[serde_json::Value],
) -> Result<Option<usize>, ServiceError> {
    // Header indices (0-based):
    // 0 domain, 1 spend_sk, 2 pk_ivk_owner, 3 depth, 4 anchor, 5 n_in
    if args.len() < 6 {
        return Err(ServiceError::ParseError(
            "Ligero args too short for note_spend_guest v2 header".to_string(),
        ));
    }

    let depth_i64 = parse_ligero_i64_arg(&args[3], "depth")?;
    let depth: usize = usize::try_from(depth_i64).map_err(|_| {
        ServiceError::ParseError(format!(
            "Invalid depth (expected non-negative i64), got {depth_i64}"
        ))
    })?;

    let n_in_i64 = parse_ligero_i64_arg(&args[5], "n_in")?;
    let n_in: usize = usize::try_from(n_in_i64).map_err(|_| {
        ServiceError::ParseError(format!(
            "Invalid n_in (expected non-negative i64), got {n_in_i64}"
        ))
    })?;
    if n_in == 0 || n_in > 4 {
        return Err(ServiceError::ParseError(format!(
            "Invalid n_in (expected 1..=4), got {n_in}"
        )));
    }

    // Walk inputs
    let mut idx: usize = 6;
    for _ in 0..n_in {
        // value_in, rho_in, sender_id_in, pos
        idx = idx
            .checked_add(4)
            .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;
        // siblings[depth]
        idx = idx
            .checked_add(depth)
            .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;
        // nullifier (public)
        idx = idx
            .checked_add(1)
            .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;
    }

    if idx + 3 > args.len() {
        return Err(ServiceError::ParseError(
            "Ligero args truncated before withdraw binding".to_string(),
        ));
    }

    let withdraw_amount_i64 = parse_ligero_i64_arg(&args[idx], "withdraw_amount")?;
    let withdraw_amount_u64: u64 = withdraw_amount_i64.try_into().map_err(|_| {
        ServiceError::ParseError(format!(
            "Invalid withdraw_amount (expected non-negative i64), got {withdraw_amount_i64}"
        ))
    })?;
    let n_out_index = idx + 2;
    let n_out_i64 = parse_ligero_i64_arg(&args[n_out_index], "n_out")?;
    let n_out: usize = usize::try_from(n_out_i64).map_err(|_| {
        ServiceError::ParseError(format!(
            "Invalid n_out (expected non-negative i64), got {n_out_i64}"
        ))
    })?;
    if n_out > 2 {
        return Err(ServiceError::ParseError(format!(
            "Invalid n_out (expected 0..=2), got {n_out}"
        )));
    }

    // Skip withdraw_to + n_out
    idx = n_out_index + 1;

    // outputs: 5 args per output
    idx = idx
        .checked_add(5 * n_out)
        .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;

    // inv_enforce
    idx = idx
        .checked_add(1)
        .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;

    // blacklist_root
    idx = idx
        .checked_add(1)
        .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;

    let checks: usize = if withdraw_amount_u64 == 0 { 2 } else { 1 };
    let bl_bucket_size: usize = midnight_privacy::BLACKLIST_BUCKET_SIZE as usize;
    let bl_depth: usize = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
    for _ in 0..checks {
        // bucket_entries + bucket_inv + siblings
        idx = idx
            .checked_add(bl_bucket_size + 1 + bl_depth)
            .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;
    }

    if idx == args.len() {
        // No viewers section present.
        return Ok(None);
    }
    if idx >= args.len() {
        return Err(ServiceError::ParseError(
            "Ligero args index past end while locating viewer section".to_string(),
        ));
    }

    let n_viewers_i64 = parse_ligero_i64_arg(&args[idx], "n_viewers")?;
    let n_viewers: usize = usize::try_from(n_viewers_i64).map_err(|_| {
        ServiceError::ParseError(format!(
            "Invalid n_viewers (expected non-negative i64), got {n_viewers_i64}"
        ))
    })?;
    if n_viewers != 1 {
        return Err(ServiceError::SignatureError(format!(
            "POOL_FVK_PK enforcement requires exactly 1 viewer (n_viewers=1), got {n_viewers}"
        )));
    }

    // Layout per viewer: fvk_commitment, fvk, then for each output ct_hash + mac.
    let expected_total = idx
        .checked_add(1 + n_viewers * (2 + 2 * n_out))
        .ok_or_else(|| ServiceError::ParseError("arg index overflow".to_string()))?;
    if expected_total != args.len() {
        return Err(ServiceError::ParseError(format!(
            "Ligero args length mismatch for viewer section: expected {expected_total}, got {}",
            args.len()
        )));
    }

    Ok(Some(idx + 1))
}

fn verify_pool_sig_over_commitment(
    pool_pk: &Ed25519VerifyingKey,
    fvk_commitment: &MidnightHash32,
    signature: &[u8; 64],
) -> Result<(), ServiceError> {
    pool_pk
        .verify_strict(fvk_commitment, &Ed25519Signature::from_bytes(signature))
        .map_err(|e| ServiceError::SignatureError(format!("Invalid pool signature: {e}")))
}

fn enforce_pool_signed_viewer_commitment_in_args(
    pool_pk: &Ed25519VerifyingKey,
    args: &[serde_json::Value],
) -> Result<MidnightHash32, ServiceError> {
    let idx = locate_viewer_fvk_commitment_index(&args)?.ok_or_else(|| {
        ServiceError::SignatureError(
            "Missing viewer section in proof args (POOL_FVK_PK is set)".to_string(),
        )
    })?;

    let fvk_commitment = decode_ligero_hash32_arg(&args[idx], "viewer.fvk_commitment")?;
    let signature = decode_pool_signature_from_ligero_arg(&args[idx], "viewer.fvk_commitment")?;

    verify_pool_sig_over_commitment(pool_pk, &fvk_commitment, &signature)?;

    Ok(fvk_commitment)
}

#[derive(Debug, Clone)]
pub struct ViewCiphertextMeta {
    cm: MidnightHash32,
    fvk_commitment: MidnightHash32,
    ct_len: usize,
}

#[derive(Debug, Clone)]
pub struct ViewCiphertextsMeta {
    notes: Vec<ViewCiphertextMeta>,
}

fn view_ciphertexts_meta(
    view_ciphertexts: Option<&Vec<EncryptedNote>>,
) -> Option<ViewCiphertextsMeta> {
    let notes = view_ciphertexts?;
    let notes = notes
        .iter()
        .map(|n| ViewCiphertextMeta {
            cm: n.cm,
            fvk_commitment: n.fvk_commitment,
            ct_len: n.ct.len(),
        })
        .collect();
    Some(ViewCiphertextsMeta { notes })
}

#[cfg(test)]
fn enforce_pool_signed_viewer_commitment(
    pool_pk: &Ed25519VerifyingKey,
    proof: &[u8],
) -> Result<MidnightHash32, ServiceError> {
    let package: sov_ligero_adapter::LigeroProofPackage =
        bincode::deserialize(proof).map_err(|e| {
            ServiceError::ParseError(format!("Proof payload is not a LigeroProofPackage ({e})"))
        })?;

    let args: Vec<serde_json::Value> = serde_json::from_slice(&package.args_json).map_err(|e| {
        ServiceError::ParseError(format!(
            "LigeroProofPackage.args_json is not valid JSON: {e}"
        ))
    })?;

    enforce_pool_signed_viewer_commitment_in_args(pool_pk, &args)
}

/// Request body for the prover service /verify endpoint
#[derive(Debug, Serialize)]
struct ProverServiceVerifyRequest {
    circuit: String,
    args: Vec<serde_json::Value>,
    proof: String, // base64-encoded
    #[serde(rename = "privateIndices")]
    private_indices: Vec<usize>,
}

/// Response from the prover service /verify endpoint
#[derive(Debug, Deserialize)]
struct ProverServiceVerifyResponse {
    success: bool,
    #[serde(rename = "exitCode")]
    exit_code: i32,
    error: Option<String>,
}

/// Verify proof using the remote ligero-http-server prover service via REST API.
///
/// This sends an HTTP POST to the prover service's /verify endpoint.
async fn verify_with_prover_service(
    http_client: &reqwest::Client,
    prover_url: &str,
    circuit: &str,
    package: &sov_ligero_adapter::LigeroProofPackage,
) -> Result<(), ServiceError> {
    let args: Vec<serde_json::Value> = serde_json::from_slice(&package.args_json)
        .map_err(|e| ServiceError::ProofError(format!("Failed to parse package args_json: {e}")))?;

    let proof_b64 = BASE64_STANDARD.encode(&package.proof);

    let request = ProverServiceVerifyRequest {
        circuit: circuit.to_string(),
        args,
        proof: proof_b64,
        private_indices: package.private_indices.clone(),
    };

    let url = format!("{}/verify", prover_url.trim_end_matches('/'));
    debug!("Sending verification request to prover service: {}", url);

    let response = http_client
        .post(&url)
        .json(&request)
        .send()
        .await
        .map_err(|e| {
            ServiceError::ProofError(format!("Failed to connect to prover service at {url}: {e}"))
        })?;

    let status = response.status();
    let body = response.text().await.map_err(|e| {
        ServiceError::ProofError(format!("Failed to read prover service response: {e}"))
    })?;

    let resp: ProverServiceVerifyResponse = serde_json::from_str(&body).map_err(|e| {
        ServiceError::ProofError(format!(
            "Failed to parse prover service response (status={}, body={}): {e}",
            status, body
        ))
    })?;

    if !resp.success {
        return Err(ServiceError::ProofError(format!(
            "Prover service verification failed (exit_code={}): {}",
            resp.exit_code,
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        )));
    }

    debug!(
        "✓ Prover service verification succeeded for circuit {}",
        circuit
    );
    Ok(())
}

/// Verify using a long-lived verifier pool hosted in a separate process.
///
/// - Uses `webgpu_verifier --daemon` worker processes managed in-process by `ligero_runner::daemon::DaemonPool`.
/// - Worker count is derived from `max_concurrent_verifications` (no daemon-specific env vars).
fn verify_with_ligero_verifier_daemon(
    commitment: &[u8; 32],
    package: &sov_ligero_adapter::LigeroProofPackage,
    workers: usize,
) -> Result<(), ServiceError> {
    use std::collections::HashMap;

    let verifier_paths =
        ligero_runner::verifier::VerifierPaths::discover_with_commitment(Some(commitment))
            .map_err(|e| {
                ServiceError::ProofError(format!("Ligero verifier config discovery failed: {e}"))
            })?;

    let args: Vec<ligero_runner::LigeroArg> = serde_json::from_slice(&package.args_json)
        .map_err(|e| ServiceError::ProofError(format!("Failed to parse package args_json: {e}")))?;

    let mut cfg = verifier_paths.to_config(args, package.private_indices.clone());

    // Proof bytes in the package may be gzip-compressed (proof_data.gz) or raw (proof_data.bin).
    // Select the correct verifier mode based on the bytes we received.
    let is_gzip = package.is_valid_gzip();
    let proof_filename = if is_gzip {
        "proof_data.gz"
    } else {
        "proof_data.bin"
    };
    cfg.gzip_proof = is_gzip;
    cfg.proof_path = Some(proof_filename.to_string());

    let cfg_json = serde_json::to_value(&cfg).map_err(|e| {
        ServiceError::ProofError(format!("Failed to serialize Ligero config JSON: {e}"))
    })?;

    // Daemon verifier expects a proof path, not raw bytes: write to temp dir.
    let dir = tempfile::tempdir()
        .map_err(|e| ServiceError::Internal(format!("Failed to create temp dir: {e}")))?;
    let proof_path = dir.path().join(proof_filename);
    std::fs::write(&proof_path, &package.proof)
        .map_err(|e| ServiceError::Internal(format!("Failed to write {proof_filename}: {e}")))?;

    // Lazily initialize (and cache) daemon pools per (verifier_bin, shader_dir).
    static POOLS: OnceLock<std::sync::Mutex<HashMap<String, ligero_runner::daemon::DaemonPool>>> =
        OnceLock::new();

    let bins_dir = verifier_paths
        .verifier_bin
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let key = format!(
        "{}|{}",
        verifier_paths.verifier_bin.display(),
        verifier_paths.shader_path.display()
    );

    let pools_lock = POOLS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let pool = {
        let mut guard = pools_lock.lock().unwrap();
        if let Some(p) = guard.get(&key) {
            p.clone()
        } else {
            info!(
                "Starting Ligero verifier daemon pool (workers={}) using verifier_bin={} shader_dir={} bins_dir={}",
                workers.max(1),
                verifier_paths.verifier_bin.display(),
                verifier_paths.shader_path.display(),
                bins_dir.display(),
            );
            let ligero_paths = ligero_runner::LigeroPaths {
                prover_bin: bins_dir.join("webgpu_prover"),
                verifier_bin: verifier_paths.verifier_bin.clone(),
                shader_dir: verifier_paths.shader_path.clone(),
                bins_dir,
            };
            let created =
                ligero_runner::daemon::DaemonPool::new_verifier(&ligero_paths, workers.max(1))
                    .map_err(|e| {
                        ServiceError::ProofError(format!(
                            "Failed to start Ligero verifier daemon pool: {e}"
                        ))
                    })?;
            guard.insert(key, created.clone());
            created
        }
    };

    let resp = pool
        .verify(cfg_json, proof_path.to_string_lossy().as_ref())
        .map_err(|e| {
            ServiceError::ProofError(format!("Ligero verifier daemon request failed: {e}"))
        })?;

    if !resp.ok {
        return Err(ServiceError::ProofError(format!(
            "Ligero verifier daemon returned ok=false (exit_code={:?}): {}",
            resp.exit_code,
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        )));
    }

    if resp.verify_ok != Some(true) {
        return Err(ServiceError::ProofError(format!(
            "Ligero verifier daemon did not confirm proof validity (verify_ok={:?})",
            resp.verify_ok
        )));
    }

    Ok(())
}

/// Request body for proof verification
#[derive(Debug, Deserialize)]
pub struct VerifyAndSubmitRequest {
    /// Base64-encoded signed transaction bytes (same format as node RPC)
    pub body: String,
}

/// Response from proof verification
#[derive(Debug, Serialize)]
pub struct VerifyAndSubmitResponse {
    /// Whether verification succeeded
    pub success: bool,
    /// Transaction hash (if successful)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    /// Raw sequencer response (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequencer_response: Option<serde_json::Value>,
    /// Error message (if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Verification timing metrics
    pub metrics: VerificationMetrics,
}

/// Timing metrics for verification
#[derive(Debug, Serialize)]
pub struct VerificationMetrics {
    /// Time to deserialize transaction (ms)
    pub deserialize_ms: f64,
    /// Time to parse transaction (ms)
    pub parse_ms: f64,
    /// Time to verify transaction signature (ms)
    pub signature_verify_ms: f64,
    /// Time to verify Ligero proof (ms)
    pub proof_verify_ms: f64,
    /// Time to create non-ZK transaction (ms)
    pub tx_creation_ms: f64,
    /// Time to submit to node (ms)
    pub node_submit_ms: f64,
    /// Total time (ms)
    pub total_ms: f64,
    /// Timestamp when the response was created (ISO 8601)
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

impl Default for VerificationMetrics {
    fn default() -> Self {
        Self {
            deserialize_ms: 0.0,
            parse_ms: 0.0,
            signature_verify_ms: 0.0,
            proof_verify_ms: 0.0,
            tx_creation_ms: 0.0,
            node_submit_ms: 0.0,
            total_ms: 0.0,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Public output from Ligero value proof
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ValueProofPublic {
    pub value: u32,
}

/// Call message for value-setter-zk module (for parsing incoming TX)
#[derive(Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSetterZkCall {
    SetValueWithProof {
        value: u32,
        #[serde(with = "serde_bytes")]
        proof: Vec<u8>,
        gas: Option<()>,
    },
    UpdateMethodId {
        new_method_id: [u8; 32],
    },
}

/// Call message for value-setter module (non-ZK) - for creating output TX
#[derive(Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSetterCall {
    SetValue { value: u32, gas: Option<()> },
    SetManyValues { values: Vec<u32>, gas: Option<()> },
}

/// Custom error type for the service
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("Failed to decode transaction: {0}")]
    DecodeError(String),

    #[error("Failed to parse transaction: {0}")]
    ParseError(String),

    #[error("Signature verification failed: {0}")]
    SignatureError(String),

    #[error("Proof verification failed: {0}")]
    ProofError(String),

    #[error("Failed to submit to node: {0}")]
    SubmissionError(String),

    #[error("Unsupported transaction: {0}")]
    UnsupportedCall(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ServiceError::DecodeError(msg) => {
                error!("Decode error: {}", msg);
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            ServiceError::ParseError(msg) => {
                error!("Parse error: {}", msg);
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            ServiceError::SignatureError(msg) => {
                error!("Signature verification error: {}", msg);
                (StatusCode::UNAUTHORIZED, msg.clone())
            }
            ServiceError::ProofError(msg) => {
                error!("Proof verification error: {}", msg);
                (StatusCode::UNPROCESSABLE_ENTITY, msg.clone())
            }
            ServiceError::SubmissionError(msg) => {
                error!("Submission error: {}", msg);
                (StatusCode::BAD_GATEWAY, msg.clone())
            }
            ServiceError::UnsupportedCall(msg) => {
                error!("Unsupported call: {}", msg);
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            ServiceError::Internal(msg) => {
                error!("Internal error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
        };

        let body = Json(serde_json::json!({
            "error": message,
            "status": status.as_u16(),
        }));

        (status, body).into_response()
    }
}

/// Create the Axum router for the service
pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/value-setter-zk", post(verify_and_submit_handler))
        .route(
            "/midnight-privacy",
            post(verify_and_record_midnight_handler),
        )
        .route("/midnight-privacy/flush", post(flush_pending_handler))
        .route(
            "/midnight-privacy/pending_count",
            get(pending_count_handler),
        )
        .route("/health", axum::routing::get(health_check))
        .with_state(state)
        // Remove default 2MB body limit and allow larger payloads.
        // Note: Midnight Ligero proof packages can be tens of MB, and transactions are submitted
        // base64-encoded (adds ~33% overhead).
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(
                    tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::DEBUG),
                )
                .on_response(
                    tower_http::trace::DefaultOnResponse::new().level(tracing::Level::DEBUG),
                ),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
}

/// Health check endpoint
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "proof-verifier",
    }))
}

#[derive(Debug, Serialize)]
struct PendingCountResponse {
    pending: u64,
}

async fn pending_count_handler(
    State(state): State<AppState>,
) -> Result<Json<PendingCountResponse>, ServiceError> {
    use sea_orm::PaginatorTrait;
    use worker_verified_transactions::{
        Column as VerifiedColumn, Entity as VerifiedEntity, TransactionState,
    };

    let pending = VerifiedEntity::find()
        .filter(VerifiedColumn::TransactionState.eq(TransactionState::Pending))
        .count(state.da_conn.as_ref())
        .await
        .map_err(|err| {
            ServiceError::Internal(format!(
                "Failed to count pending worker transactions: {err}"
            ))
        })?;

    Ok(Json(PendingCountResponse { pending }))
}

#[derive(Debug, Deserialize)]
struct FlushQuery {
    limit: Option<u64>,
}

/// Flush all pending worker-verified transactions to the sequencer in parallel.
///
/// This returns as soon as all sequencer submissions have completed and the
/// aggregate results are computed; the per-tx DB updates are applied in a
/// background task so they don't block the HTTP response.
async fn flush_pending_handler(
    State(state): State<AppState>,
    Query(query): Query<FlushQuery>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    use sea_orm::{QueryOrder, QuerySelect};
    use worker_verified_transactions::{
        Column as VerifiedColumn, Entity as VerifiedEntity, TransactionState,
    };

    // Fetch list of pending tx hashes (only the tx_hash column, to avoid loading large blobs)
    let mut pending_query = VerifiedEntity::find()
        .select_only()
        .column(VerifiedColumn::TxHash)
        .filter(VerifiedColumn::TransactionState.eq(TransactionState::Pending))
        .order_by_asc(VerifiedColumn::Id);

    if let Some(limit) = query.limit {
        pending_query = pending_query.limit(limit);
    }

    let pending_tx_hashes: Vec<String> = pending_query
        .into_tuple::<(String,)>()
        .all(state.da_conn.as_ref())
        .await
        .map_err(|err| {
            ServiceError::Internal(format!("Failed to list pending worker transactions: {err}"))
        })?
        .into_iter()
        .map(|(txh,)| txh)
        .collect();

    let total = pending_tx_hashes.len();
    if total == 0 {
        return Ok(Json(serde_json::json!({
            "flushed": 0,
            "accepted": 0,
            "rejected": 0,
            "results": []
        })));
    }

    let mut handles = Vec::with_capacity(total);

    // Limit the number of concurrent submissions to the sequencer to avoid
    // overloading the preferred sequencer's single-threaded message loop.
    // This helps reduce per-tx submit_ms/await_ms while still processing the
    // whole batch efficiently.
    const MAX_FLUSH_CONCURRENCY: usize = 32;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_FLUSH_CONCURRENCY));

    for txh in pending_tx_hashes {
        let st = state.clone();
        let sem = semaphore.clone();
        handles.push(tokio::spawn(async move {
            // Acquire a permit to enforce the concurrency limit.
            let _permit = sem.acquire_owned().await.expect("flush semaphore closed");

            // Only submit to sequencer and collect the outcome in memory.
            // Database updates are applied after all submissions complete.
            let res = send_worker_tx_to_sequencer(&st, &txh).await;
            (txh, res)
        }));
    }

    // Await all handles in parallel
    let sequencer_start = std::time::Instant::now();
    let all_results = join_all(handles).await;
    let sequencer_elapsed_ms = sequencer_start.elapsed().as_secs_f64() * 1000.0;
    info!(
        "Sequencer processed worker transactions in {:.2} ms",
        sequencer_elapsed_ms
    );

    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut results = Vec::new();
    let mut db_updates: Vec<(String, SequencerSubmissionOutcome)> = Vec::new();

    for outcome in all_results {
        match outcome {
            Ok((txh, Ok(outcome))) => {
                // Queue this tx for background DB update.
                let txh_for_db = txh.clone();

                // Update aggregate counts based on sequencer outcome.
                if outcome.accepted {
                    accepted += 1;
                } else {
                    rejected += 1;
                }

                // Build a breakdown object ensuring all expected timing keys exist.
                let mut breakdown_value = outcome
                    .internal_breakdown
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({}));
                if let serde_json::Value::Object(ref mut map) = breakdown_value {
                    let total_fallback = outcome.internal_ms.unwrap_or(outcome.latency_ms);
                    map.entry("total_ms")
                        .or_insert_with(|| serde_json::Value::from(total_fallback));
                    map.entry("decode_ms")
                        .or_insert_with(|| serde_json::Value::from(0.0_f64));
                    map.entry("wrap_ms")
                        .or_insert_with(|| serde_json::Value::from(0.0_f64));
                    map.entry("submit_ms")
                        .or_insert_with(|| serde_json::Value::from(total_fallback));
                    map.entry("await_ms")
                        .or_insert_with(|| serde_json::Value::from(0.0_f64));
                    map.entry("stf_execution_ms")
                        .or_insert_with(|| serde_json::Value::Null);
                }

                results.push(serde_json::json!({
                    "tx_hash": txh.clone(),
                    "accepted": outcome.accepted,
                    "status": outcome.status_code,
                    "response": outcome.response_json.clone(),
                    // Prefer internal sequencer processing time if available; fall back to HTTP latency.
                    "sequencer_ms": outcome.internal_ms.unwrap_or(outcome.latency_ms),
                    "sequencer_breakdown": breakdown_value,
                }));

                db_updates.push((txh_for_db, outcome));
            }
            Ok((txh, Err(err))) => {
                rejected += 1;
                results.push(serde_json::json!({
                    "tx_hash": txh,
                    "accepted": false,
                    "error": format!("{}", err),
                }));
            }
            Err(join_err) => {
                rejected += 1;
                results.push(serde_json::json!({
                    "tx_hash": null,
                    "accepted": false,
                    "error": format!("join error: {}", join_err),
                }));
            }
        }
    }

    // Apply DB updates in the background so the HTTP response isn't blocked on
    // SQLite/Postgres write latency. Errors are logged but do not affect the
    // response.
    if !db_updates.is_empty() {
        let db_conn = state.da_conn.clone();
        tokio::spawn(async move {
            use sea_orm::TransactionTrait;

            let txn_res = db_conn.begin().await;
            let Ok(txn) = txn_res else {
                if let Err(err) = txn_res {
                    error!(
                        "Failed to begin transaction for worker tx updates in background: {}",
                        err
                    );
                }
                return;
            };

            for (txh, outcome) in db_updates {
                if let Err(err) =
                    update_worker_tx_after_submission_in_conn(&txn, &txh, &outcome).await
                {
                    error!(
                        tx_hash = %txh,
                        "Failed to update worker transaction after sequencer submission in background: {}",
                        err
                    );
                }
            }

            if let Err(err) = txn.commit().await {
                error!("Failed to commit worker tx updates in background: {}", err);
            }
        });
    }

    info!(
        "Total worker transactions processed: {total}, accepted: {accepted}, rejected: {rejected}"
    );

    Ok(Json(serde_json::json!({
        "flushed": total,
        "accepted": accepted,
        "rejected": rejected,
        "results": results,
    })))
}

/// Main handler for verify-and-submit endpoint
async fn verify_and_submit_handler(
    State(state): State<AppState>,
    Json(req): Json<VerifyAndSubmitRequest>,
) -> Result<Json<VerifyAndSubmitResponse>, ServiceError> {
    let start = std::time::Instant::now();
    let mut metrics = VerificationMetrics::default();

    debug!("Received verification request");

    // Acquire semaphore permit to limit concurrent verifications
    let _permit = state
        .verification_semaphore
        .acquire()
        .await
        .map_err(|e| ServiceError::Internal(format!("Semaphore error: {}", e)))?;

    debug!("Acquired verification permit, starting processing");

    // Step 1: Decode the base64 transaction bytes
    let decode_start = std::time::Instant::now();
    let tx_bytes = BASE64_STANDARD
        .decode(&req.body)
        .map_err(|e| ServiceError::DecodeError(format!("Invalid base64: {}", e)))?;
    metrics.deserialize_ms = decode_start.elapsed().as_secs_f64() * 1000.0;

    // Step 2: Parse the transaction to extract value and proof
    // Note: We're using a simplified approach here. In production, you'd deserialize
    // the full Transaction<Runtime, Spec> type, but that requires knowing the Runtime type.
    // For now, we'll work with the raw transaction bytes and extract what we need.
    let parse_start = std::time::Instant::now();
    let (value, proof) = parse_value_setter_zk_transaction(&tx_bytes)?;
    metrics.parse_ms = parse_start.elapsed().as_secs_f64() * 1000.0;

    debug!(
        "Parsed transaction: value={}, proof_size={} bytes",
        value,
        proof.len()
    );

    // Step 3: Verify Ligero proof (in parallel)
    let proof_start = std::time::Instant::now();
    verify_ligero_proof(&state, value, &proof).await?;
    metrics.proof_verify_ms = proof_start.elapsed().as_secs_f64() * 1000.0;

    debug!("✓ Proof verification successful for value={}", value);

    // Step 4: Create and sign non-ZK transaction
    let tx_start = std::time::Instant::now();
    let signed_tx_bytes = create_and_sign_non_zk_transaction(&state, value).await?;
    metrics.tx_creation_ms = tx_start.elapsed().as_secs_f64() * 1000.0;

    // Step 5: Submit to node
    let submit_start = std::time::Instant::now();
    let tx_hash = submit_to_node(&state, signed_tx_bytes).await?;
    metrics.node_submit_ms = submit_start.elapsed().as_secs_f64() * 1000.0;

    metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

    debug!(
        "✓ Successfully processed transaction: value={}, hash={}, total_time={:.2}ms",
        value, tx_hash, metrics.total_ms
    );

    Ok(Json(VerifyAndSubmitResponse {
        success: true,
        tx_hash: Some(tx_hash),
        sequencer_response: None,
        error: None,
        metrics,
    }))
}

async fn verify_and_record_midnight_handler(
    State(state): State<AppState>,
    Json(req): Json<VerifyAndSubmitRequest>,
) -> Result<Json<VerifyAndSubmitResponse>, ServiceError> {
    let start = std::time::Instant::now();
    let mut metrics = VerificationMetrics::default();

    debug!("Received midnight transaction verification request");

    let _permit = state
        .verification_semaphore
        .acquire()
        .await
        .map_err(|e| ServiceError::Internal(format!("Semaphore error: {}", e)))?;

    let decode_start = std::time::Instant::now();
    let tx_bytes = BASE64_STANDARD
        .decode(&req.body)
        .map_err(|e| ServiceError::DecodeError(format!("Invalid base64: {}", e)))?;
    metrics.deserialize_ms = decode_start.elapsed().as_secs_f64() * 1000.0;

    let parse_start = std::time::Instant::now();
    let tx: Transaction<DemoRuntime<RollupSpec>, RollupSpec> =
        borsh::BorshDeserialize::try_from_slice(&tx_bytes).map_err(|e| {
            let err = e.to_string();
            let hint = if err.contains("Unexpected length of input") {
                " (often indicates the verifier was compiled with a smaller SafeVec/proof limit than the submitted tx)"
            } else {
                ""
            };
            ServiceError::ParseError(format!(
                "Failed to deserialize transaction (body_b64_len={}, tx_bytes_len={}): {}{}",
                req.body.len(),
                tx_bytes.len(),
                err,
                hint
            ))
        })?;

    let parsed_call = parse_midnight_call(&tx)?;
    metrics.parse_ms = parse_start.elapsed().as_secs_f64() * 1000.0;

    let signature_start = std::time::Instant::now();
    verify_midnight_transaction_signature(&tx, &state.rollup_chain_hash)?;
    metrics.signature_verify_ms = signature_start.elapsed().as_secs_f64() * 1000.0;

    let tx_hash = tx.hash().to_string();
    let transaction_data = create_transaction_without_proof(&tx)?;

    async fn handle_no_proof_midnight_call(
        state: &AppState,
        tx: &DemoTransaction,
        tx_hash: &str,
        transaction_data: &str,
        full_transaction_blob: &str,
        start: std::time::Instant,
        mut metrics: VerificationMetrics,
    ) -> Result<Json<VerifyAndSubmitResponse>, ServiceError> {
        info!(
            tx_hash,
            da_connection = %redact_db_connection_string(&state.config.da_connection_string),
            node_rpc_url = %state.config.node_rpc_url,
            defer_submission = state.config.defer_sequencer_submission,
            "handle_no_proof_midnight_call: starting persist+submit flow for deposit/no-proof tx"
        );

        let persist_start = std::time::Instant::now();
        // Populate serialized_tx_base64 so sequencer flush path works uniformly.
        let pre_auth_data = match extract_pre_authenticated_data(tx) {
            Ok(data) => {
                debug!(
                    tx_hash,
                    "✓ Extracted pre-authenticated data for no-proof call"
                );
                Some(data)
            }
            Err(e) => {
                error!("⚠️  Failed to extract pre-authenticated data for midnight no-proof call: {e}; falling back to base64 body only");
                Some((
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    full_transaction_blob.to_string(),
                ))
            }
        };

        store_verified_midnight_transaction(
            state.da_conn.as_ref(),
            &state.incoming_worker_tx_saver,
            tx_hash,
            "{}".to_string(), // No proof outputs for no-proof calls
            None,             // No view_attestations_json
            true,             // signature_valid
            None,             // proof_verified: NULL (transaction doesn't have a proof)
            transaction_data,
            full_transaction_blob,
            pre_auth_data,
            None, // No encrypted notes (no view_ciphertexts)
        )
        .await?;
        let persist_ms = persist_start.elapsed().as_secs_f64() * 1000.0;
        metrics.tx_creation_ms = persist_ms;
        metrics.proof_verify_ms = 0.0;
        info!(
            tx_hash,
            persist_ms = format!("{:.2}", persist_ms),
            "handle_no_proof_midnight_call: persist completed, proceeding to sequencer submission"
        );

        if state.config.defer_sequencer_submission {
            info!(tx_hash, "handle_no_proof_midnight_call: defer mode — skipping immediate sequencer submission");
            // Do not submit now; queued in DB
            metrics.node_submit_ms = 0.0;
            metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
            return Ok(Json(VerifyAndSubmitResponse {
                success: true,
                tx_hash: Some(tx_hash.to_string()),
                sequencer_response: None,
                error: None,
                metrics,
            }));
        }

        info!(
            tx_hash,
            node_base_url = %state.node_client.base_url,
            "handle_no_proof_midnight_call: submitting worker tx to sequencer (immediate mode)"
        );
        let sequencer_start = std::time::Instant::now();
        let submission = submit_worker_tx_to_sequencer(state, tx_hash).await?;
        metrics.node_submit_ms = sequencer_start.elapsed().as_secs_f64() * 1000.0;
        metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

        info!(
            tx_hash,
            accepted = submission.accepted,
            status_code = ?submission.status_code,
            sequencer_latency_ms = format!("{:.2}", submission.latency_ms),
            total_ms = format!("{:.2}", metrics.total_ms),
            "handle_no_proof_midnight_call: sequencer submission completed"
        );

        let error_message = if submission.accepted {
            None
        } else {
            Some(submission.log_message.clone())
        };

        Ok(Json(VerifyAndSubmitResponse {
            success: submission.accepted,
            tx_hash: Some(tx_hash.to_string()),
            sequencer_response: submission.response_json.clone(),
            error: error_message,
            metrics,
        }))
    }

    match parsed_call {
        ParsedMidnightCall::Deposit {
            amount,
            rho,
            recipient,
            view_fvks,
        } => {
            // Deposits don't have proofs, so we just verify signature and store
            debug!(
                "Parsed midnight deposit: amount={}, rho=0x{}, recipient=0x{}, view_fvks={:?}",
                amount,
                hex::encode(&rho[..8]),
                hex::encode(&recipient[..8]),
                view_fvks.as_ref().map(|v| v.len()),
            );
            handle_no_proof_midnight_call(
                &state,
                &tx,
                &tx_hash,
                &transaction_data,
                &req.body,
                start,
                metrics,
            )
            .await
        }
        ParsedMidnightCall::Transfer {
            proof,
            anchor_root,
            nullifiers,
            view_ciphertexts,
        } => {
            // Transfers have proofs but zero withdraw amount; outputs contain new commitments
            debug!(
                "Parsed midnight transfer: n_nullifiers={}, anchor_root=0x{}, proof_size={} bytes",
                nullifiers.len(),
                hex::encode(anchor_root),
                proof.len()
            );

            let ciphertexts_meta = view_ciphertexts_meta(view_ciphertexts.as_ref());
            let proof_start = std::time::Instant::now();
            // For transfers, expected withdraw_amount is 0
            let proof_public = verify_midnight_withdraw_proof(
                state.config.midnight_method_id.as_ref(),
                proof,
                state.config.max_concurrent_verifications,
                anchor_root,
                &nullifiers,
                0u128,
                state.pool_fvk_pk.clone(),
                ciphertexts_meta,
                state.config.prover_service_url.as_deref(),
                Some(&state.http_client),
            )
            .await?;
            metrics.proof_verify_ms = proof_start.elapsed().as_secs_f64() * 1000.0;

            let persist_start = std::time::Instant::now();
            // Extract pre-authenticated data for optimized sequencer processing
            let pre_auth_data = match extract_pre_authenticated_data(&tx) {
                Ok(data) => {
                    debug!("✓ Extracted pre-authenticated data for transfer (includes lightweight tx without proof)");
                    Some(data)
                }
                Err(e) => {
                    error!("⚠️  Failed to extract pre-authenticated data for transfer: {e}");
                    None
                }
            };

            let proof_outputs_json = serde_json::to_string(&proof_public).map_err(|err| {
                ServiceError::Internal(format!("Failed to serialize proof outputs: {err}"))
            })?;
            let view_attestations_json = proof_public
                .view_attestations
                .as_ref()
                .map(|atts| {
                    serde_json::to_string(atts).map_err(|err| {
                        ServiceError::Internal(format!(
                            "Failed to serialize view_attestations: {err}"
                        ))
                    })
                })
                .transpose()?;

            store_verified_midnight_transaction(
                state.da_conn.as_ref(),
                &state.incoming_worker_tx_saver,
                &tx_hash,
                proof_outputs_json,
                view_attestations_json,
                true,       // signature_valid
                Some(true), // proof_verified: true
                &transaction_data,
                &req.body,
                pre_auth_data,
                view_ciphertexts.as_ref(), // Level-B encrypted notes for authority viewing
            )
            .await?;
            metrics.tx_creation_ms = persist_start.elapsed().as_secs_f64() * 1000.0;
            debug!(
                "✓ Stored verified midnight transfer: n_nullifiers={}, anchor_root=0x{}, hash={}, encrypted_notes={}",
                nullifiers.len(),
                hex::encode(anchor_root),
                tx_hash,
                view_ciphertexts.as_ref().map(|v| v.len()).unwrap_or(0)
            );

            if state.config.defer_sequencer_submission {
                // Do not submit now; queued in DB
                metrics.node_submit_ms = 0.0;
                metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
                Ok(Json(VerifyAndSubmitResponse {
                    success: true,
                    tx_hash: Some(tx_hash),
                    sequencer_response: None,
                    error: None,
                    metrics,
                }))
            } else {
                let sequencer_start = std::time::Instant::now();
                let submission = submit_worker_tx_to_sequencer(&state, &tx_hash).await?;
                metrics.node_submit_ms = sequencer_start.elapsed().as_secs_f64() * 1000.0;
                metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

                let error_message = if submission.accepted {
                    None
                } else {
                    Some(submission.log_message.clone())
                };

                Ok(Json(VerifyAndSubmitResponse {
                    success: submission.accepted,
                    tx_hash: Some(tx_hash),
                    sequencer_response: submission.response_json.clone(),
                    error: error_message,
                    metrics,
                }))
            }
        }
        ParsedMidnightCall::Withdraw {
            proof,
            anchor_root,
            nullifier,
            withdraw_amount,
            to,
            view_ciphertexts,
        } => {
            // Withdrawals have proofs that need verification
            debug!(
                "Parsed midnight withdrawal: nullifier=0x{}, anchor_root=0x{}, withdraw_amount={}, proof_size={} bytes, view_ciphertexts={:?}",
                hex::encode(nullifier),
                hex::encode(anchor_root),
                withdraw_amount,
                proof.len(),
                view_ciphertexts.as_ref().map(|v| v.len()),
            );

            let ciphertexts_meta = view_ciphertexts_meta(view_ciphertexts.as_ref());
            let proof_start = std::time::Instant::now();
            let proof_public = verify_midnight_withdraw_proof(
                state.config.midnight_method_id.as_ref(),
                proof,
                state.config.max_concurrent_verifications,
                anchor_root,
                std::slice::from_ref(&nullifier),
                withdraw_amount,
                state.pool_fvk_pk.clone(),
                ciphertexts_meta,
                state.config.prover_service_url.as_deref(),
                Some(&state.http_client),
            )
            .await?;
            metrics.proof_verify_ms = proof_start.elapsed().as_secs_f64() * 1000.0;

            let persist_start = std::time::Instant::now();
            // Extract pre-authenticated data for optimized sequencer processing
            let pre_auth_data = match extract_pre_authenticated_data(&tx) {
                Ok(data) => {
                    debug!("✓ Extracted pre-authenticated data for withdraw (includes lightweight tx without proof)");
                    Some(data)
                }
                Err(e) => {
                    error!("⚠️  Failed to extract pre-authenticated data for withdraw: {e}");
                    None
                }
            };
            let proof_outputs_json = serde_json::to_string(&proof_public).map_err(|err| {
                ServiceError::Internal(format!("Failed to serialize proof outputs: {err}"))
            })?;
            let view_attestations_json = proof_public
                .view_attestations
                .as_ref()
                .map(|atts| {
                    serde_json::to_string(atts).map_err(|err| {
                        ServiceError::Internal(format!(
                            "Failed to serialize view_attestations: {err}"
                        ))
                    })
                })
                .transpose()?;
            store_verified_midnight_transaction(
                state.da_conn.as_ref(),
                &state.incoming_worker_tx_saver,
                &tx_hash,
                proof_outputs_json,
                view_attestations_json,
                true,       // signature_valid
                Some(true), // proof_verified: true (has proof and verified correctly)
                &transaction_data,
                &req.body,
                pre_auth_data,
                view_ciphertexts.as_ref(), // Level-B encrypted notes for authority viewing
            )
            .await?;
            metrics.tx_creation_ms = persist_start.elapsed().as_secs_f64() * 1000.0;
            debug!(
                "✓ Stored verified midnight withdrawal: nullifier=0x{}, anchor_root=0x{}, withdraw_amount={}, to={:?}, hash={}, encrypted_notes={}",
                hex::encode(nullifier),
                hex::encode(anchor_root),
                withdraw_amount,
                to,
                tx_hash,
                view_ciphertexts.as_ref().map(|v| v.len()).unwrap_or(0)
            );

            if state.config.defer_sequencer_submission {
                // Do not submit now; queued in DB
                metrics.node_submit_ms = 0.0;
                metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
                Ok(Json(VerifyAndSubmitResponse {
                    success: true,
                    tx_hash: Some(tx_hash),
                    sequencer_response: None,
                    error: None,
                    metrics,
                }))
            } else {
                let sequencer_start = std::time::Instant::now();
                let submission = submit_worker_tx_to_sequencer(&state, &tx_hash).await?;
                metrics.node_submit_ms = sequencer_start.elapsed().as_secs_f64() * 1000.0;
                metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

                let error_message = if submission.accepted {
                    None
                } else {
                    Some(submission.log_message.clone())
                };

                Ok(Json(VerifyAndSubmitResponse {
                    success: submission.accepted,
                    tx_hash: Some(tx_hash),
                    sequencer_response: submission.response_json.clone(),
                    error: error_message,
                    metrics,
                }))
            }
        }
        ParsedMidnightCall::UpdateMethodId { new_method_id } => {
            debug!(
                "Parsed midnight update_method_id: new_method_id=0x{}",
                hex::encode(new_method_id)
            );
            handle_no_proof_midnight_call(
                &state,
                &tx,
                &tx_hash,
                &transaction_data,
                &req.body,
                start,
                metrics,
            )
            .await
        }
        ParsedMidnightCall::FreezeAddress { address } => {
            debug!(
                "Parsed midnight freeze_address: address={}",
                address.to_string()
            );
            handle_no_proof_midnight_call(
                &state,
                &tx,
                &tx_hash,
                &transaction_data,
                &req.body,
                start,
                metrics,
            )
            .await
        }
        ParsedMidnightCall::UnfreezeAddress { address } => {
            debug!(
                "Parsed midnight unfreeze_address: address={}",
                address.to_string()
            );
            handle_no_proof_midnight_call(
                &state,
                &tx,
                &tx_hash,
                &transaction_data,
                &req.body,
                start,
                metrics,
            )
            .await
        }
        ParsedMidnightCall::AddPoolAdmin { admin } => {
            debug!(
                "Parsed midnight add_pool_admin: admin={}",
                admin.to_string()
            );
            handle_no_proof_midnight_call(
                &state,
                &tx,
                &tx_hash,
                &transaction_data,
                &req.body,
                start,
                metrics,
            )
            .await
        }
        ParsedMidnightCall::RemovePoolAdmin { admin } => {
            debug!(
                "Parsed midnight remove_pool_admin: admin={}",
                admin.to_string()
            );
            handle_no_proof_midnight_call(
                &state,
                &tx,
                &tx_hash,
                &transaction_data,
                &req.body,
                start,
                metrics,
            )
            .await
        }
    }
}

/// Parse value-setter-zk transaction to extract value and proof
///
/// Deserializes the transaction and extracts the runtime call to get the claimed value and proof.
///
/// SECURITY MODEL:
/// - This parser extracts the `claimed_value` from the transaction
/// - The WASM program enforces `proven_value == claimed_value`
/// - If the claimed value is fraudulent, Ligero verification will fail
/// - The cryptographic guarantee comes from the WASM program, not this parser
fn parse_value_setter_zk_transaction(tx_bytes: &[u8]) -> Result<(u32, Vec<u8>), ServiceError> {
    // First, deserialize the full transaction to get access to the runtime_call
    let tx: Transaction<DemoRuntime<RollupSpec>, RollupSpec> =
        borsh::BorshDeserialize::try_from_slice(tx_bytes).map_err(|e| {
            ServiceError::ParseError(format!("Failed to deserialize transaction: {}", e))
        })?;

    // Get the runtime call (this is the Runtime::Call enum)
    let runtime_call = tx.runtime_call();

    // Now we need to extract the value and proof from the runtime call
    // Since Runtime::Call is an auto-generated enum, we can't pattern match on it directly
    // Instead, serialize it and parse the borsh bytes
    let call_bytes = borsh::to_vec(runtime_call).map_err(|e| {
        ServiceError::ParseError(format!("Failed to serialize runtime call: {}", e))
    })?;

    // Now parse the call bytes to extract value and proof
    // This is more reliable than parsing the full transaction bytes
    parse_ligero_call_bytes(&call_bytes)
}

/// Parse the runtime call bytes to extract value and proof from SetValueWithProof
///
/// This searches for the proof blob (large ~3.2MB) and extracts the u32 value
/// that appears immediately before it in the borsh-encoded structure.
///
/// The borsh encoding of SetValueWithProof { value, proof, ... } is:
/// - enum variant index (module selector, 1-4 bytes)
/// - enum variant index (call selector, 1-4 bytes)  
/// - value: u32 (4 bytes) <- IMMEDIATELY BEFORE proof
/// - proof: Vec<u8> = length (4 bytes) + data (~3.2MB)
fn parse_ligero_call_bytes(bytes: &[u8]) -> Result<(u32, Vec<u8>), ServiceError> {
    // Search for the proof length marker followed by a large proof
    // Ligero proofs are ~3.2MB, so look for a u32 length marker of that size
    //
    // The borsh encoding of SetValueWithProof is:
    // - enum variant index (1-4 bytes)
    // - value: u32 (4 bytes) <- IMMEDIATELY BEFORE proof
    // - proof: Vec<u8> = length (4 bytes) + data
    // - (other fields like gas, etc.)

    const MIN_PROOF_SIZE: u32 = 3_000_000; // 3 MB minimum
    const MAX_PROOF_SIZE: u32 = 5_000_000; // 5 MB maximum

    for i in 0..bytes.len().saturating_sub(4) {
        // Read 4 bytes as little-endian u32 (borsh Vec length encoding)
        let len = u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);

        if len >= MIN_PROOF_SIZE && len <= MAX_PROOF_SIZE {
            // Found a candidate proof length at offset i
            let proof_start = i + 4;
            let proof_end = proof_start + len as usize;

            if proof_end <= bytes.len() && i >= 4 {
                let proof = bytes[proof_start..proof_end].to_vec();

                // The value should be the u32 IMMEDIATELY BEFORE the proof length marker
                // So it's at offset (i - 4)
                let value_offset = i.saturating_sub(4);
                let value = u32::from_le_bytes([
                    bytes[value_offset],
                    bytes[value_offset + 1],
                    bytes[value_offset + 2],
                    bytes[value_offset + 3],
                ]);

                // Value should be in range [0, 65535] (u16) for our use case
                if value <= 65535 {
                    debug!(
                        "Heuristic parser found: value={} at offset={}, proof size={} bytes at offset={}",
                        value,
                        value_offset,
                        proof.len(),
                        proof_start
                    );
                    return Ok((value, proof));
                } else {
                    // If value is out of range, this might not be the right proof marker
                    // Continue searching
                    debug!(
                        "Found proof-sized blob at offset {} but value {} is out of range [0, 65535], continuing search",
                        i,
                        value
                    );
                    continue;
                }
            }
        }
    }

    Err(ServiceError::ParseError(
        "Could not find value and proof in transaction".to_string(),
    ))
}

/// Verify Ligero proof using either the remote prover service or local daemon
async fn verify_ligero_proof(
    state: &AppState,
    value: u32,
    proof: &[u8],
) -> Result<(), ServiceError> {
    let method_id_bytes = state.config.value_setter_method_id.ok_or_else(|| {
        ServiceError::Internal(
            "Value-setter method ID not configured. \
            The service needs the value_validator_rust.wasm program to compute the method ID."
                .to_string(),
        )
    })?;
    let method_id = LigeroCodeCommitment(method_id_bytes);
    let workers = state.config.max_concurrent_verifications;
    let prover_url = state.config.prover_service_url.clone();
    let http_client = state.http_client.clone();

    let proof = proof.to_vec();

    // Decode package first (needed for both paths)
    let package: sov_ligero_adapter::LigeroProofPackage =
        bincode::deserialize(&proof).map_err(|err| {
            ServiceError::ProofError(format!(
                "Proof payload is not a LigeroProofPackage ({}). \
                     Regenerate the proof with the updated tooling.",
                err
            ))
        })?;

    debug!(
        "Ligero proof package decoded: proof_bytes={} public_output_bytes={}",
        package.proof.len(),
        package.public_output.len()
    );

    if ligero_skip_verify_enabled() {
        // Skip verification, just decode public output
    } else if let Some(url) = prover_url {
        // Use remote prover service
        debug!(
            "Using remote prover service at {} for value-setter verification",
            url
        );
        verify_with_prover_service(&http_client, &url, "value_validator_rust", &package).await?;
    } else {
        // Fall back to local daemon pool
        debug!("Using local daemon pool for value-setter verification");
        let package_clone = package.clone();
        tokio::task::spawn_blocking(move || {
            verify_with_ligero_verifier_daemon(&method_id.0, &package_clone, workers)
        })
        .await
        .map_err(|e| ServiceError::Internal(format!("Task join error: {}", e)))??;
    }

    // Decode and verify public output
    let public: ValueProofPublic = bincode::deserialize(&package.public_output)
        .map_err(|e| ServiceError::ProofError(format!("Failed to decode public output: {e}")))?;

    // Check that the public output matches the claimed value
    if public.value != value {
        return Err(ServiceError::ProofError(format!(
            "Value mismatch: claimed={}, verified={}",
            value, public.value
        )));
    }

    Ok(())
}

/// Create and sign a non-ZK value-setter transaction
///
/// This creates a Transaction<Runtime, Spec> with value-setter::SetValue call
/// and signs it with the service's private key.
///
/// Uses a local nonce counter to avoid nonce conflicts when processing multiple
/// requests concurrently.
async fn create_and_sign_non_zk_transaction(
    state: &AppState,
    value: u32,
) -> Result<Vec<u8>, ServiceError> {
    // Use the cached signing key
    let signing_key = &*state.signing_key;

    // Get the next nonce using synchronized counter
    let nonce = {
        let mut nonce_guard = state.nonce_counter.lock().await;

        // If this is the first request, fetch the current nonce from the node
        if nonce_guard.is_none() {
            let pub_key = signing_key.pub_key();
            let current_nonce = state
                .node_client
                .get_nonce_for_public_key::<RollupSpec>(&pub_key)
                .await
                .map_err(|e| ServiceError::Internal(format!("Failed to get nonce: {}", e)))?;

            debug!("Initialized nonce counter from node: {}", current_nonce);
            *nonce_guard = Some(current_nonce);
        }

        // Get current nonce and increment for next transaction
        let nonce = nonce_guard.unwrap();
        *nonce_guard = Some(nonce + 1);

        nonce
    }; // Mutex is released here

    debug!("Creating non-ZK transaction with nonce={}", nonce);
    debug!("  Chain ID: {}", state.config.chain_id);
    debug!(
        "  Using rollup chain hash from /rollup/schema: {}",
        hex::encode(&state.rollup_chain_hash)
    );

    // Create the transaction
    // Note: This is a simplified version. In production, you'd use the actual
    // Runtime::Call type and properly construct the transaction.

    // For now, we'll use a generic approach that creates a transaction structure
    // that matches what sov-cli would create

    let signed_tx_bytes = create_value_setter_tx_bytes(
        value,
        nonce,
        signing_key,
        state.config.chain_id,
        &state.rollup_chain_hash,
    )
    .map_err(|e| {
        error!("Failed to create transaction bytes: {}", e);
        ServiceError::Internal(format!("Failed to create transaction: {}", e))
    })?;

    debug!(
        "Created signed transaction: {} bytes",
        signed_tx_bytes.len()
    );

    Ok(signed_tx_bytes)
}

/// Helper to create a value-setter transaction bytes
/// This mimics what sov-cli does when creating transactions
fn create_value_setter_tx_bytes(
    value: u32,
    nonce: u64,
    signing_key: &<<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey,
    chain_id: u64,
    chain_hash: &[u8; 32],
) -> Result<Vec<u8>> {
    // Create the runtime call for value-setter
    use sov_value_setter::CallMessage as ValueSetterCallMessage;

    let value_setter_call = ValueSetterCallMessage::<RollupSpec>::SetValue { value, gas: None };

    let runtime_call = RuntimeCall::ValueSetter(value_setter_call);

    // Create transaction details
    let details = TxDetails {
        max_fee: Amount::from(100_000_000_000u128),
        max_priority_fee_bips: PriorityFeeBips(0),
        gas_limit: None, // Let the system calculate gas automatically
        chain_id,
    };

    // Create unsigned transaction (UniquenessData is an enum with Generation variant)
    let uniqueness = UniquenessData::Generation(nonce);
    let unsigned_tx = UnsignedTransaction::new_with_details(runtime_call, uniqueness, details);

    // Debug: serialize unsigned tx to see what we're signing
    let unsigned_tx_bytes = borsh::to_vec(&unsigned_tx)?;
    debug!(
        "Unsigned transaction bytes ({} bytes): 0x{}",
        unsigned_tx_bytes.len(),
        hex::encode(&unsigned_tx_bytes)
    );
    debug!("CHAIN_HASH for signing: 0x{}", hex::encode(chain_hash));

    // Sign the transaction using the rollup chain hash (from /rollup/schema)
    let signed_tx = Transaction::<DemoRuntime<RollupSpec>, RollupSpec>::new_signed_tx(
        signing_key,
        chain_hash,
        unsigned_tx,
    );

    // Serialize to bytes
    let tx_bytes = borsh::to_vec(&signed_tx)?;

    debug!("Created signed transaction: {} bytes", tx_bytes.len());
    debug!("  Signing key pub_key: {:?}", signing_key.pub_key());
    debug!("  FULL TRANSACTION HEX: 0x{}", hex::encode(&tx_bytes));

    Ok(tx_bytes)
}

/// Parsed midnight transaction data
enum ParsedMidnightCall {
    Deposit {
        amount: u128,
        rho: MidnightHash32,
        recipient: MidnightHash32,
        view_fvks: Option<Vec<FullViewingKey>>,
    },
    Transfer {
        proof: Vec<u8>,
        anchor_root: MidnightHash32,
        nullifiers: Vec<MidnightHash32>,
        view_ciphertexts: Option<Vec<EncryptedNote>>,
    },
    Withdraw {
        proof: Vec<u8>,
        anchor_root: MidnightHash32,
        nullifier: MidnightHash32,
        withdraw_amount: u128,
        to: <RollupSpec as Spec>::Address,
        view_ciphertexts: Option<Vec<EncryptedNote>>,
    },
    UpdateMethodId {
        new_method_id: [u8; 32],
    },
    FreezeAddress {
        address: midnight_privacy::PrivacyAddress,
    },
    UnfreezeAddress {
        address: midnight_privacy::PrivacyAddress,
    },
    AddPoolAdmin {
        admin: <RollupSpec as Spec>::Address,
    },
    RemovePoolAdmin {
        admin: <RollupSpec as Spec>::Address,
    },
}

fn parse_midnight_call(
    tx: &Transaction<DemoRuntime<RollupSpec>, RollupSpec>,
) -> Result<ParsedMidnightCall, ServiceError> {
    match tx.runtime_call() {
        RuntimeCall::MidnightPrivacy(call) => match call.clone() {
            MidnightCallMessage::Deposit {
                amount,
                rho,
                recipient,
                view_fvks,
                ..
            } => Ok(ParsedMidnightCall::Deposit {
                amount,
                rho,
                recipient,
                view_fvks,
            }),
            MidnightCallMessage::Transfer {
                proof,
                anchor_root,
                nullifiers,
                view_ciphertexts,
                ..
            } => Ok(ParsedMidnightCall::Transfer {
                proof: proof.into(),
                anchor_root,
                nullifiers,
                view_ciphertexts,
            }),
            MidnightCallMessage::Withdraw {
                proof,
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                view_ciphertexts,
                ..
            } => Ok(ParsedMidnightCall::Withdraw {
                proof: proof.into(),
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                view_ciphertexts,
            }),
            MidnightCallMessage::UpdateMethodId { new_method_id } => {
                Ok(ParsedMidnightCall::UpdateMethodId { new_method_id })
            }
            MidnightCallMessage::FreezeAddress { address } => {
                Ok(ParsedMidnightCall::FreezeAddress { address })
            }
            MidnightCallMessage::UnfreezeAddress { address } => {
                Ok(ParsedMidnightCall::UnfreezeAddress { address })
            }
            MidnightCallMessage::AddPoolAdmin { admin } => {
                Ok(ParsedMidnightCall::AddPoolAdmin { admin })
            }
            MidnightCallMessage::RemovePoolAdmin { admin } => {
                Ok(ParsedMidnightCall::RemovePoolAdmin { admin })
            }
        },
        other => Err(ServiceError::UnsupportedCall(format!(
            "Expected midnight_privacy call, got {other:?}"
        ))),
    }
}

pub fn parse_midnight_withdraw_call(
    tx: &Transaction<DemoRuntime<RollupSpec>, RollupSpec>,
) -> Result<
    (
        Vec<u8>,
        MidnightHash32,
        MidnightHash32,
        u128,
        <RollupSpec as Spec>::Address,
        Option<Vec<EncryptedNote>>,
    ),
    ServiceError,
> {
    match parse_midnight_call(tx)? {
        ParsedMidnightCall::Withdraw {
            proof,
            anchor_root,
            nullifier,
            withdraw_amount,
            to,
            view_ciphertexts,
        } => Ok((
            proof,
            anchor_root,
            nullifier,
            withdraw_amount,
            to,
            view_ciphertexts,
        )),
        ParsedMidnightCall::Deposit { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got Deposit".to_string(),
        )),
        ParsedMidnightCall::Transfer { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got Transfer".to_string(),
        )),
        ParsedMidnightCall::UpdateMethodId { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got UpdateMethodId".to_string(),
        )),
        ParsedMidnightCall::FreezeAddress { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got FreezeAddress".to_string(),
        )),
        ParsedMidnightCall::UnfreezeAddress { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got UnfreezeAddress".to_string(),
        )),
        ParsedMidnightCall::AddPoolAdmin { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got AddPoolAdmin".to_string(),
        )),
        ParsedMidnightCall::RemovePoolAdmin { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got RemovePoolAdmin".to_string(),
        )),
    }
}

pub fn verify_midnight_transaction_signature(
    tx: &Transaction<DemoRuntime<RollupSpec>, RollupSpec>,
    chain_hash: &[u8; 32],
) -> Result<(), ServiceError> {
    let mut meter = UnlimitedGasMeter::<RollupSpec>::default();
    tx.verify(chain_hash, &mut meter)
        .map_err(|e| ServiceError::SignatureError(e.to_string()))
}

pub async fn verify_midnight_withdraw_proof(
    method_id_opt: Option<&[u8; 32]>,
    proof: Vec<u8>,
    workers: usize,
    expected_anchor_root: MidnightHash32,
    expected_nullifiers: &[MidnightHash32],
    expected_withdraw_amount: u128,
    pool_fvk_pk: Option<Ed25519VerifyingKey>,
    view_ciphertexts_meta: Option<ViewCiphertextsMeta>,
    prover_service_url: Option<&str>,
    http_client: Option<&reqwest::Client>,
) -> Result<SpendPublic, ServiceError> {
    let method_id_bytes = method_id_opt.ok_or_else(|| {
        ServiceError::Internal(
            "Midnight method ID not configured. \
            The service needs the note_spend_guest.wasm program to compute the method ID."
                .to_string(),
        )
    })?;
    let method_id = LigeroCodeCommitment(*method_id_bytes);
    let proof_vec = proof;

    // Decode package first
    let package: sov_ligero_adapter::LigeroProofPackage = bincode::deserialize(&proof_vec)
        .map_err(|err| {
            ServiceError::ProofError(format!(
                "Proof payload is not a LigeroProofPackage ({}). \
                 Regenerate the proof with the updated tooling.",
                err
            ))
        })?;

    debug!(
        "Midnight proof package decoded: proof_bytes={} public_output_bytes={}",
        package.proof.len(),
        package.public_output.len()
    );

    // Verify pool signature over viewer commitment before doing expensive proof verification
    let expected_viewer_fvk_commitment = match pool_fvk_pk.as_ref() {
        Some(pool_pk) => {
            let args: Vec<serde_json::Value> =
                serde_json::from_slice(&package.args_json).map_err(|e| {
                    ServiceError::ParseError(format!(
                        "LigeroProofPackage.args_json is not valid JSON: {e}"
                    ))
                })?;
            let expected = enforce_pool_signed_viewer_commitment_in_args(pool_pk, &args)?;

            let meta = view_ciphertexts_meta.as_ref().ok_or_else(|| {
                ServiceError::ProofError(
                    "POOL_FVK_PK is set: Transfer/Withdraw tx must include view_ciphertexts (encrypted note payload bytes)".to_string(),
                )
            })?;
            if meta.notes.is_empty() {
                return Err(ServiceError::ProofError(
                    "POOL_FVK_PK is set: view_ciphertexts must be non-empty".to_string(),
                ));
            }
            for (idx, note) in meta.notes.iter().enumerate() {
                if note.ct_len == 0 {
                    return Err(ServiceError::ProofError(format!(
                        "POOL_FVK_PK is set: view_ciphertexts[{idx}].ct is empty"
                    )));
                }
                if note.fvk_commitment != expected {
                    return Err(ServiceError::ProofError(format!(
                        "POOL_FVK_PK is set: view_ciphertexts[{idx}].fvk_commitment (0x{}) != signed viewer commitment (0x{})",
                        hex::encode(note.fvk_commitment),
                        hex::encode(expected)
                    )));
                }
            }

            Some(expected)
        }
        None => None,
    };

    // Perform ZK proof verification
    if ligero_skip_verify_enabled() {
        // Skip verification
    } else if let (Some(url), Some(client)) = (prover_service_url, http_client) {
        // Use remote prover service
        debug!(
            "Using remote prover service at {} for midnight verification",
            url
        );
        verify_with_prover_service(client, url, "note_spend_guest", &package).await?;
    } else {
        // Fall back to local daemon pool
        debug!("Using local daemon pool for midnight verification");
        let package_clone = package.clone();
        tokio::task::spawn_blocking(move || {
            verify_with_ligero_verifier_daemon(&method_id.0, &package_clone, workers)
        })
        .await
        .map_err(|e| ServiceError::Internal(format!("Task join error: {}", e)))??;
    }

    // Decode and verify public output
    let public: SpendPublic = bincode::deserialize(&package.public_output)
        .map_err(|e| ServiceError::ProofError(format!("Failed to decode public output: {e}")))?;

    // Verify public output against expected values
    if public.anchor_root != expected_anchor_root {
        return Err(ServiceError::ProofError(format!(
            "Anchor root mismatch: expected 0x{}, proof 0x{}",
            hex::encode(expected_anchor_root),
            hex::encode(public.anchor_root)
        )));
    }
    if public.nullifiers.as_slice() != expected_nullifiers {
        return Err(ServiceError::ProofError(format!(
            "Nullifiers mismatch: expected {:?}, proof {:?}",
            expected_nullifiers
                .iter()
                .map(|n| format!("0x{}", hex::encode(n)))
                .collect::<Vec<_>>(),
            public
                .nullifiers
                .iter()
                .map(|n| format!("0x{}", hex::encode(n)))
                .collect::<Vec<_>>(),
        )));
    }
    if public.withdraw_amount != expected_withdraw_amount {
        return Err(ServiceError::ProofError(format!(
            "Withdraw amount mismatch: expected {}, proof {}",
            expected_withdraw_amount, public.withdraw_amount
        )));
    }

    // Verify pool FVK viewer commitments if enabled
    if let Some(expected_fvk_c) = expected_viewer_fvk_commitment {
        let atts = public.view_attestations.as_ref().ok_or_else(|| {
            ServiceError::ProofError(
                "Expected proof to include view_attestations (POOL_FVK_PK is set)".to_string(),
            )
        })?;
        if !atts.iter().any(|a| a.fvk_commitment == expected_fvk_c) {
            return Err(ServiceError::ProofError(format!(
                "view_attestations missing expected fvk_commitment 0x{}",
                hex::encode(expected_fvk_c)
            )));
        }

        let meta = view_ciphertexts_meta.as_ref().ok_or_else(|| {
            ServiceError::ProofError(
                "POOL_FVK_PK is set: Transfer/Withdraw tx must include view_ciphertexts (encrypted note payload bytes)".to_string(),
            )
        })?;

        let outputs_len = public.output_commitments.len();
        if outputs_len != meta.notes.len() {
            return Err(ServiceError::ProofError(format!(
                "POOL_FVK_PK is set: expected {} view_ciphertexts (one per output commitment), got {}",
                outputs_len,
                meta.notes.len()
            )));
        }

        use std::collections::HashSet;
        let output_set: HashSet<MidnightHash32> =
            public.output_commitments.iter().copied().collect();
        let ciphertext_set: HashSet<MidnightHash32> = meta.notes.iter().map(|n| n.cm).collect();

        if output_set.len() != outputs_len {
            return Err(ServiceError::ProofError(
                "Proof output_commitments contains duplicate commitments".to_string(),
            ));
        }
        if ciphertext_set.len() != meta.notes.len() {
            return Err(ServiceError::ProofError(
                "view_ciphertexts contains duplicate commitments".to_string(),
            ));
        }

        if output_set != ciphertext_set {
            let missing: Vec<String> = public
                .output_commitments
                .iter()
                .filter(|cm| !ciphertext_set.contains(*cm))
                .map(|cm| format!("0x{}", hex::encode(cm)))
                .collect();
            let extra: Vec<String> = meta
                .notes
                .iter()
                .filter(|n| !output_set.contains(&n.cm))
                .map(|n| format!("0x{}", hex::encode(n.cm)))
                .collect();
            return Err(ServiceError::ProofError(format!(
                "POOL_FVK_PK is set: view_ciphertexts/output_commitments mismatch (missing={missing:?}, extra={extra:?})"
            )));
        }

        for cm in &public.output_commitments {
            if !atts
                .iter()
                .any(|a| a.cm == *cm && a.fvk_commitment == expected_fvk_c)
            {
                return Err(ServiceError::ProofError(format!(
                    "POOL_FVK_PK is set: view_attestations missing (cm=0x{}, fvk_commitment=0x{})",
                    hex::encode(cm),
                    hex::encode(expected_fvk_c),
                )));
            }
        }
    }

    Ok(public)
}

/// Create a transaction JSON representation without the proof data
/// Extracts the runtime call message and replaces proof with "REMOVED"
pub fn create_transaction_without_proof(tx: &DemoTransaction) -> Result<String, ServiceError> {
    // Extract the runtime call and create JSON representation with proof removed
    match tx.runtime_call() {
        RuntimeCall::MidnightPrivacy(call) => {
            let call_json = match call.clone() {
                MidnightCallMessage::Deposit {
                    amount,
                    rho,
                    recipient,
                    view_fvks,
                    gas,
                } => {
                    serde_json::json!({
                        "deposit": {
                            "amount": amount.to_string(),
                            "rho": hex::encode(rho),
                            "recipient": format!("{:?}", recipient),
                            "view_fvks": view_fvks,
                            "gas": gas
                        }
                    })
                }
                MidnightCallMessage::Transfer {
                    anchor_root,
                    nullifiers,
                    view_ciphertexts,
                    gas,
                    ..
                } => {
                    let nullifiers: Vec<String> =
                        nullifiers.iter().map(|n| hex::encode(n)).collect();
                    serde_json::json!({
                        "transfer": {
                            "proof": "REMOVED",
                            "anchor_root": hex::encode(anchor_root),
                            "nullifiers": nullifiers,
                            "view_ciphertexts": view_ciphertexts,
                            "gas": gas
                        }
                    })
                }
                MidnightCallMessage::Withdraw {
                    anchor_root,
                    nullifier,
                    withdraw_amount,
                    to,
                    view_ciphertexts,
                    gas,
                    ..
                } => {
                    serde_json::json!({
                        "withdraw": {
                            "proof": "REMOVED",
                            "anchor_root": hex::encode(anchor_root),
                            "nullifier": hex::encode(nullifier),
                            "withdraw_amount": withdraw_amount.to_string(),
                            "to": format!("{:?}", to),
                            "view_ciphertexts": view_ciphertexts,
                            "gas": gas
                        }
                    })
                }
                MidnightCallMessage::UpdateMethodId { new_method_id } => {
                    serde_json::json!({
                        "update_method_id": {
                            "new_method_id": hex::encode(new_method_id)
                        }
                    })
                }
                MidnightCallMessage::FreezeAddress { address } => {
                    serde_json::json!({
                        "freeze_address": {
                            "address": address.to_string()
                        }
                    })
                }
                MidnightCallMessage::UnfreezeAddress { address } => {
                    serde_json::json!({
                        "unfreeze_address": {
                            "address": address.to_string()
                        }
                    })
                }
                MidnightCallMessage::AddPoolAdmin { admin } => {
                    serde_json::json!({
                        "add_pool_admin": {
                            "admin": admin.to_string()
                        }
                    })
                }
                MidnightCallMessage::RemovePoolAdmin { admin } => {
                    serde_json::json!({
                        "remove_pool_admin": {
                            "admin": admin.to_string()
                        }
                    })
                }
            };

            serde_json::to_string(&call_json)
                .map_err(|e| ServiceError::Internal(format!("Failed to serialize JSON: {}", e)))
        }
        RuntimeCall::ValueSetter(call) => {
            use sov_value_setter::CallMessage as ValueSetterCallMessage;
            let call_json = match call.clone() {
                ValueSetterCallMessage::SetValue { value, gas } => {
                    serde_json::json!({
                        "set_value": {
                            "value": value,
                            "gas": gas
                        }
                    })
                }
                ValueSetterCallMessage::SetManyValues(values) => {
                    serde_json::json!({
                        "set_many_values": values
                    })
                }
                ValueSetterCallMessage::AssertVisibleSlotNumber {
                    expected_visible_slot_number,
                } => {
                    serde_json::json!({
                        "assert_visible_slot_number": {
                            "expected_visible_slot_number": expected_visible_slot_number
                        }
                    })
                }
                ValueSetterCallMessage::SetValueAndSleep {
                    value,
                    sleep_millis,
                } => {
                    serde_json::json!({
                        "set_value_and_sleep": {
                            "value": value,
                            "sleep_millis": sleep_millis
                        }
                    })
                }
                ValueSetterCallMessage::Panic => {
                    serde_json::json!({
                        "panic": {}
                    })
                }
            };

            serde_json::to_string(&call_json)
                .map_err(|e| ServiceError::Internal(format!("Failed to serialize JSON: {}", e)))
        }
        other => {
            // For other call types, provide a basic representation
            let call_json = serde_json::json!({
                "type": format!("{:?}", other),
                "note": "Call data not fully serialized"
            });
            serde_json::to_string(&call_json)
                .map_err(|e| ServiceError::Internal(format!("Failed to serialize JSON: {}", e)))
        }
    }
}

/// Extract pre-authenticated transaction components to avoid re-processing the 3MB blob
/// All components are borsh-serialized and hex-encoded for reliable reconstruction
fn extract_pre_authenticated_data(
    tx: &Transaction<DemoRuntime<RollupSpec>, RollupSpec>,
) -> Result<(String, String, String, String, String), ServiceError> {
    use sov_modules_api::transaction::{Version0, VersionedTx};

    match &tx.versioned_tx {
        VersionedTx::V0(v0) => {
            // Serialize all components as borsh bytes then hex encode
            // This ensures we can reliably reconstruct them without type issues
            let pub_key_hex = hex::encode(&borsh::to_vec(&v0.pub_key).map_err(|e| {
                ServiceError::Internal(format!("Failed to serialize public key: {}", e))
            })?);

            let signature_hex = hex::encode(&borsh::to_vec(&v0.signature).map_err(|e| {
                ServiceError::Internal(format!("Failed to serialize signature: {}", e))
            })?);

            let uniqueness_hex = hex::encode(&borsh::to_vec(&v0.uniqueness).map_err(|e| {
                ServiceError::Internal(format!("Failed to serialize uniqueness: {}", e))
            })?);

            let details_hex = hex::encode(&borsh::to_vec(&v0.details).map_err(|e| {
                ServiceError::Internal(format!("Failed to serialize details: {}", e))
            })?);

            // OPTIMIZATION: Create a lightweight transaction WITHOUT the proof for pre-authenticated path
            //
            // SECURITY MODEL:
            // 1. Worker verifies signature on FULL transaction (with 3MB proof) ✓
            // 2. Worker verifies the Ligero proof itself ✓
            // 3. Worker computes hash of FULL transaction (this is what signature was computed over)
            // 4. Worker creates LIGHTWEIGHT transaction (proof stripped, saves ~3MB transfer)
            // 5. Worker stores: lightweight_tx + original_hash + proof_outputs
            // 6. Sequencer receives: lightweight_tx + original_hash
            // 7. Sequencer uses PreAuthenticated wrapper with original_hash
            // 8. STF uses the provided hash (NO recalculation) and skips signature verification
            //
            // The signature is cryptographically bound to the FULL transaction content,
            // but we don't need to verify it again since the worker already did.
            // The STF just needs to know the correct hash for tracking purposes.

            let lightweight_runtime_call = {
                use demo_stf::runtime::RuntimeCall;
                use midnight_privacy::CallMessage;
                use sov_modules_api::SafeVec;

                match &v0.runtime_call {
                    RuntimeCall::MidnightPrivacy(midnight_call) => {
                        match midnight_call {
                            CallMessage::Withdraw {
                                proof: _,
                                anchor_root,
                                nullifier,
                                withdraw_amount,
                                to,
                                view_ciphertexts,
                                gas,
                            } => {
                                // Create withdraw call with EMPTY proof (already verified by worker)
                                RuntimeCall::MidnightPrivacy(CallMessage::Withdraw {
                                    proof: SafeVec::new(), // EMPTY! Saves ~3MB transfer
                                    anchor_root: *anchor_root,
                                    nullifier: *nullifier,
                                    withdraw_amount: *withdraw_amount,
                                    to: to.clone(),
                                    view_ciphertexts: view_ciphertexts.clone(),
                                    gas: gas.clone(),
                                })
                            }
                            CallMessage::Transfer {
                                proof: _,
                                anchor_root,
                                nullifiers,
                                view_ciphertexts,
                                gas,
                            } => {
                                // Create transfer call with EMPTY proof (already verified by worker)
                                RuntimeCall::MidnightPrivacy(CallMessage::Transfer {
                                    proof: SafeVec::new(), // EMPTY! Saves ~3MB transfer
                                    anchor_root: *anchor_root,
                                    nullifiers: nullifiers.clone(),
                                    view_ciphertexts: view_ciphertexts.clone(),
                                    gas: gas.clone(),
                                })
                            }
                            _ => {
                                // For non-proof calls (deposits, etc), keep as-is
                                v0.runtime_call.clone()
                            }
                        }
                    }
                    _ => v0.runtime_call.clone(),
                }
            };

            // Create lightweight transaction (proof stripped, saves ~3MB)
            let lightweight_tx = Transaction::<DemoRuntime<RollupSpec>, RollupSpec> {
                versioned_tx: VersionedTx::V0(Version0 {
                    signature: v0.signature.clone(),
                    pub_key: v0.pub_key.clone(),
                    runtime_call: lightweight_runtime_call,
                    uniqueness: v0.uniqueness.clone(),
                    details: v0.details.clone(),
                }),
            };

            // Serialize the LIGHTWEIGHT transaction (proof stripped, saves ~3MB)
            let lightweight_tx_bytes = borsh::to_vec(&lightweight_tx).map_err(|e| {
                ServiceError::Internal(format!(
                    "Failed to serialize lightweight transaction: {}",
                    e
                ))
            })?;
            let serialized_tx_base64 =
                base64::engine::general_purpose::STANDARD.encode(&lightweight_tx_bytes);

            // Compute size savings
            let full_tx_bytes = borsh::to_vec(tx).map_err(|e| {
                ServiceError::Internal(format!("Failed to serialize full transaction: {}", e))
            })?;
            let bytes_saved = full_tx_bytes.len() - lightweight_tx_bytes.len();

            debug!(
                "✓ Created lightweight pre-authenticated transaction: {} bytes (original: {} bytes, saved: {} bytes)",
                lightweight_tx_bytes.len(),
                full_tx_bytes.len(),
                bytes_saved
            );

            Ok((
                pub_key_hex,
                signature_hex,
                uniqueness_hex,
                details_hex,
                serialized_tx_base64,
            ))
        }
    }
}

pub async fn store_verified_midnight_transaction(
    conn: &DatabaseConnection,
    incoming_worker_tx_saver: &IncomingWorkerTxSaver,
    tx_hash: &str,
    proof_outputs_json: String,
    view_attestations_json: Option<String>,
    signature_valid: bool,
    proof_verified: Option<bool>,
    transaction_data: &str,
    full_transaction_blob: &str,
    pre_auth_data: Option<(String, String, String, String, String)>,
    encrypted_notes: Option<&Vec<EncryptedNote>>,
) -> Result<(), ServiceError> {
    use worker_verified_transactions::{
        ActiveModel as VerifiedActiveModel, Column as VerifiedColumn, Entity as VerifiedEntity,
        TransactionState,
    };

    let full_transaction_location = match incoming_worker_tx_saver
        .save(tx_hash, full_transaction_blob)
        .await
    {
        Ok(location) => location,
        Err(err) => {
            warn!(
                tx_hash,
                ?err,
                "Failed to persist full incoming worker transaction blob"
            );
            None
        }
    };

    // Extract optional viewing data
    let view_fvks_json =
        (|| -> Option<Result<String, ServiceError>> {
            let v: serde_json::Value = serde_json::from_str(transaction_data).ok()?;
            let fvks = v.get("deposit")?.get("view_fvks")?.clone();
            Some(serde_json::to_string(&fvks).map_err(|err| {
                ServiceError::Internal(format!("Failed to serialize view_fvks: {err}"))
            }))
        })()
        .transpose()?;
    // Serialize encrypted notes to JSON for Level-B viewing access
    let encrypted_notes_json = encrypted_notes
        .map(|notes| {
            serde_json::to_string(notes).map_err(|err| {
                ServiceError::Internal(format!("Failed to serialize encrypted notes: {err}"))
            })
        })
        .transpose()?;

    // Extract pre-authenticated data if available
    let has_pre_auth = pre_auth_data.is_some();
    let (pub_key_hex, signature_hex, uniqueness_hex, details_hex, serialized_tx_base64) =
        match pre_auth_data {
            Some((pk, sig, uq, det, ser_tx)) => (
                Set(Some(pk)),
                Set(Some(sig)),
                Set(Some(uq)),
                Set(Some(det)),
                Set(Some(ser_tx)),
            ),
            None => (Set(None), Set(None), Set(None), Set(None), Set(None)),
        };
    // Derive sender address (and withdraw recipient, if any) from the full transaction blob (base64-encoded borsh Transaction)
    let (sender_str, recipient_str) = (|| -> Result<(String, Option<String>), ServiceError> {
        let raw = BASE64_STANDARD
            .decode(full_transaction_blob.as_bytes())
            .map_err(|e| ServiceError::Internal(format!("Failed to decode base64 tx blob: {e}")))?;
        let tx: DemoTransaction = borsh::BorshDeserialize::try_from_slice(&raw)
            .map_err(|e| ServiceError::Internal(format!("Failed to parse tx blob: {e}")))?;
        let sender_addr: <RollupSpec as Spec>::Address = match &tx.versioned_tx {
            sov_modules_api::transaction::VersionedTx::V0(inner) => {
                let cred = inner.pub_key.credential_id();
                cred.into()
            }
        };
        let recipient = match tx.runtime_call() {
            demo_stf::runtime::RuntimeCall::MidnightPrivacy(
                midnight_privacy::CallMessage::Withdraw { to, .. },
            ) => Some(to.to_string()),
            _ => None,
        };
        Ok((sender_addr.to_string(), recipient))
    })()?;

    let db_backend = conn.get_database_backend();
    info!(
        tx_hash,
        db_backend = ?db_backend,
        signature_valid,
        proof_verified = ?proof_verified,
        sender = %sender_str,
        recipient = ?recipient_str,
        has_pre_auth,
        has_full_tx_location = full_transaction_location.is_some(),
        "Persisting worker transaction to DA database"
    );

    let persist_start = std::time::Instant::now();
    VerifiedEntity::insert(VerifiedActiveModel {
        tx_hash: Set(tx_hash.to_owned()),
        signature_valid: Set(signature_valid),
        proof_verified: Set(proof_verified),
        transaction_data: Set(transaction_data.to_owned()),
        proof_outputs: Set(proof_outputs_json),
        view_fvks_json: Set(view_fvks_json),
        view_attestations_json: Set(view_attestations_json),
        encrypted_notes_json: Set(encrypted_notes_json),
        pub_key_hex,
        signature_hex,
        uniqueness_hex,
        details_hex,
        serialized_tx_base64,
        full_transaction_location: Set(full_transaction_location),
        transaction_state: Set(TransactionState::Pending),
        sequencer_status: Set(None),
        sender: Set(sender_str),
        recipient: Set(recipient_str),
        created_at: Set(Utc::now()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(VerifiedColumn::TxHash)
            .update_columns([
                VerifiedColumn::SignatureValid,
                VerifiedColumn::ProofVerified,
                VerifiedColumn::TransactionData,
                VerifiedColumn::ProofOutputs,
                VerifiedColumn::ViewFvksJson,
                VerifiedColumn::ViewAttestationsJson,
                VerifiedColumn::EncryptedNotesJson,
                VerifiedColumn::PubKeyHex,
                VerifiedColumn::SignatureHex,
                VerifiedColumn::UniquenessHex,
                VerifiedColumn::DetailsHex,
                VerifiedColumn::SerializedTxBase64,
                VerifiedColumn::FullTransactionLocation,
                VerifiedColumn::TransactionState,
                VerifiedColumn::SequencerStatus,
                VerifiedColumn::Sender,
                VerifiedColumn::Recipient,
                VerifiedColumn::CreatedAt,
            ])
            .to_owned(),
    )
    .exec(conn)
    .await
    .map_err(|err| {
        error!(
            tx_hash,
            db_backend = ?db_backend,
            error = %err,
            "Failed to store verified transaction in DA database"
        );
        ServiceError::Internal(format!("Failed to store verified transaction: {err}"))
    })?;

    let persist_ms = persist_start.elapsed().as_secs_f64() * 1000.0;
    info!(
        tx_hash,
        db_backend = ?db_backend,
        persist_ms = format!("{:.2}", persist_ms),
        "✓ Worker transaction persisted in DA database"
    );

    Ok(())
}

#[derive(Debug)]
struct SequencerSubmissionOutcome {
    accepted: bool,
    status_code: Option<u16>,
    raw_response: Option<String>,
    response_json: Option<serde_json::Value>,
    log_message: String,
    /// End-to-end HTTP latency for the sequencer submission (ms)
    latency_ms: f64,
    /// Optional internal processing time reported by the sequencer (ms)
    internal_ms: Option<f64>,
    /// Optional internal breakdown metrics reported by the sequencer
    internal_breakdown: Option<serde_json::Value>,
}

/// Send a worker-verified transaction to the sequencer and return its outcome
/// without mutating the shared MockDA database.
async fn send_worker_tx_to_sequencer(
    state: &AppState,
    tx_hash: &str,
) -> Result<SequencerSubmissionOutcome, ServiceError> {
    let url = format!(
        "{}/sequencer/worker_txs/{}",
        state.node_client.base_url, tx_hash
    );
    info!(
        tx_hash,
        url = %url,
        node_base_url = %state.node_client.base_url,
        da_connection = %redact_db_connection_string(&state.config.da_connection_string),
        "Sending worker transaction to sequencer via POST"
    );
    let http_start = std::time::Instant::now();
    let http_result = state.http_client.post(&url).send().await;
    let latency_ms = http_start.elapsed().as_secs_f64() * 1000.0;

    let outcome = match http_result {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.map_err(|err| {
                ServiceError::Internal(format!("Failed to read sequencer response: {err}"))
            })?;

            let parsed_json = serde_json::from_str(&body).ok();

            // Try to extract internal timing metrics from the sequencer response.
            // Prefer the dedicated `sequencer_metrics` object when available,
            // otherwise fall back to a generic `metrics` field or top-level totals.
            let (internal_ms, internal_breakdown) = match &parsed_json {
                Some(serde_json::Value::Object(map)) => {
                    if let Some(metrics) = map.get("sequencer_metrics") {
                        let total_ms = metrics.get("total_ms").and_then(|v| v.as_f64());
                        (total_ms, Some(metrics.clone()))
                    } else if let Some(metrics) = map.get("metrics") {
                        let total_ms = metrics.get("total_ms").and_then(|v| v.as_f64());
                        (total_ms, Some(metrics.clone()))
                    } else {
                        let total_ms = map.get("total_ms").and_then(|v| v.as_f64());
                        (total_ms, parsed_json.clone())
                    }
                }
                _ => (None, None),
            };

            if status.is_success() {
                info!(
                    tx_hash,
                    status = status.as_u16(),
                    latency_ms = format!("{:.2}", latency_ms),
                    internal_ms = ?internal_ms,
                    "✓ Sequencer accepted worker transaction via POST"
                );
                SequencerSubmissionOutcome {
                    accepted: true,
                    status_code: Some(status.as_u16()),
                    raw_response: Some(body.clone()),
                    response_json: parsed_json,
                    log_message: body,
                    latency_ms,
                    internal_ms,
                    internal_breakdown,
                }
            } else {
                error!(
                    tx_hash,
                    status = status.as_u16(),
                    url = %url,
                    latency_ms = format!("{:.2}", latency_ms),
                    response_body = %body,
                    "Sequencer worker-tx endpoint returned non-success"
                );
                if status == reqwest::StatusCode::NOT_FOUND {
                    use worker_verified_transactions::{
                        Column as VerifiedColumn, Entity as VerifiedEntity,
                    };

                    let local_db = redact_db_connection_string(&state.config.da_connection_string);
                    let local_backend = state.da_conn.get_database_backend();

                    error!(
                        tx_hash,
                        local_db = %local_db,
                        local_backend = ?local_backend,
                        sequencer_response = %body,
                        "404 from sequencer — diagnosing: is the tx in our local DA DB?"
                    );

                    match VerifiedEntity::find()
                        .filter(VerifiedColumn::TxHash.eq(tx_hash.to_string()))
                        .one(state.da_conn.as_ref())
                        .await
                    {
                        Ok(Some(model)) => {
                            error!(
                                tx_hash,
                                local_db = %local_db,
                                local_backend = ?local_backend,
                                local_state = ?model.transaction_state,
                                local_created_at = %model.created_at,
                                local_signature_valid = model.signature_valid,
                                local_proof_verified = ?model.proof_verified,
                                local_sender = %model.sender,
                                local_recipient = ?model.recipient,
                                local_has_serialized_tx = model.serialized_tx_base64.is_some(),
                                local_has_pub_key = model.pub_key_hex.is_some(),
                                local_sequencer_status = ?model.sequencer_status,
                                local_full_tx_location = ?model.full_transaction_location,
                                "DIAGNOSTIC: Worker transaction EXISTS in verifier DA DB but sequencer returned 404. \
                                 This strongly suggests verifier and sequencer are NOT sharing the same DA database, \
                                 or the sequencer is reading from a different table/schema."
                            );
                        }
                        Ok(None) => {
                            error!(
                                tx_hash,
                                local_db = %local_db,
                                local_backend = ?local_backend,
                                "DIAGNOSTIC: Worker transaction MISSING in verifier DA DB as well — \
                                 the insert likely failed silently or was rolled back"
                            );
                        }
                        Err(err) => {
                            error!(
                                tx_hash,
                                local_db = %local_db,
                                local_backend = ?local_backend,
                                error = %err,
                                "DIAGNOSTIC: Failed to query verifier DA DB after sequencer 404"
                            );
                        }
                    }
                }
                let body_value = parsed_json
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::String(body.clone()));
                let response_value = serde_json::json!({
                    "status": status.as_u16(),
                    "body": body_value,
                });
                let payload = response_value.to_string();

                SequencerSubmissionOutcome {
                    accepted: false,
                    status_code: Some(status.as_u16()),
                    raw_response: Some(payload),
                    response_json: Some(response_value.clone()),
                    log_message: body,
                    latency_ms,
                    internal_ms,
                    internal_breakdown,
                }
            }
        }
        Err(err) => {
            let message = format!("Failed to reach sequencer endpoint: {err}");
            error!(
                tx_hash,
                url = %url,
                latency_ms = format!("{:.2}", latency_ms),
                error = %err,
                "Network error contacting sequencer — is the sequencer running and reachable?"
            );
            let json_value = serde_json::json!({ "error": message });
            SequencerSubmissionOutcome {
                accepted: false,
                status_code: None,
                raw_response: Some(json_value.to_string()),
                response_json: Some(json_value),
                log_message: message,
                latency_ms,
                internal_ms: None,
                internal_breakdown: None,
            }
        }
    };

    Ok(outcome)
}

fn redact_db_connection_string(s: &str) -> String {
    let Some(scheme_end) = s.find("://") else {
        return s.to_string();
    };
    let (scheme, rest) = s.split_at(scheme_end + 3);

    // Only treat `userinfo@...` as such if the '@' appears before any '/' or '?'.
    let Some(at_pos) = rest.find('@') else {
        return s.to_string();
    };
    let slash_pos = rest.find('/').unwrap_or(rest.len());
    let q_pos = rest.find('?').unwrap_or(rest.len());
    let end_userinfo = slash_pos.min(q_pos);
    if at_pos > end_userinfo {
        return s.to_string();
    }

    let userinfo = &rest[..at_pos];
    let after = &rest[at_pos + 1..];
    let Some(colon_pos) = userinfo.find(':') else {
        return s.to_string();
    };
    let user = &userinfo[..colon_pos];
    format!("{scheme}{user}:***@{after}")
}

/// Update the worker transaction record in the shared MockDA database after
/// receiving the sequencer outcome.
async fn update_worker_tx_after_submission(
    state: &AppState,
    tx_hash: &str,
    outcome: &SequencerSubmissionOutcome,
) -> Result<(), ServiceError> {
    update_worker_tx_after_submission_in_conn(state.da_conn.as_ref(), tx_hash, outcome).await
}

/// Update the worker transaction record using a generic connection (can be a
/// pooled connection or a transaction).
async fn update_worker_tx_after_submission_in_conn<C>(
    conn: &C,
    tx_hash: &str,
    outcome: &SequencerSubmissionOutcome,
) -> Result<(), ServiceError>
where
    C: ConnectionTrait,
{
    use worker_verified_transactions::{
        ActiveModel as VerifiedActiveModel, Column as VerifiedColumn, Entity as VerifiedEntity,
        TransactionState,
    };

    let db_backend = conn.get_database_backend();
    debug!(
        tx_hash,
        accepted = outcome.accepted,
        db_backend = ?db_backend,
        "update_worker_tx_after_submission: fetching record from DA DB to update state"
    );

    let record = VerifiedEntity::find()
        .filter(VerifiedColumn::TxHash.eq(tx_hash))
        .one(conn)
        .await
        .map_err(|err| {
            error!(
                tx_hash,
                db_backend = ?db_backend,
                error = %err,
                "update_worker_tx_after_submission: failed to fetch worker tx from DA DB"
            );
            ServiceError::Internal(format!(
                "Failed to fetch worker transaction {tx_hash} before sequencer submission: {err}"
            ))
        })?
        .ok_or_else(|| {
            error!(
                tx_hash,
                db_backend = ?db_backend,
                "update_worker_tx_after_submission: worker tx NOT FOUND in DA DB after persisting — \
                 row may have been deleted or insert was rolled back"
            );
            ServiceError::Internal(format!(
                "Worker transaction {tx_hash} not found after persisting"
            ))
        })?;

    let new_state = if outcome.accepted {
        TransactionState::Accepted
    } else {
        TransactionState::Rejected
    };
    debug!(
        tx_hash,
        old_state = ?record.transaction_state,
        new_state = ?new_state,
        "update_worker_tx_after_submission: updating transaction state"
    );

    let mut active_model: VerifiedActiveModel = record.into();
    active_model.transaction_state = Set(new_state);
    active_model.sequencer_status = Set(outcome.raw_response.clone());
    // Use `created_at` as the "state transition timestamp" so metrics like peak TPS
    // reflect *when* the sequencer processed the tx (accepted/rejected), not when it
    // was first inserted as pending.
    active_model.created_at = Set(Utc::now());
    active_model.update(conn).await.map_err(|err| {
        error!(
            tx_hash,
            db_backend = ?db_backend,
            error = %err,
            "update_worker_tx_after_submission: failed to update worker tx in DA DB"
        );
        ServiceError::Internal(format!(
            "Failed to update worker transaction {tx_hash} after sequencer submission: {err}"
        ))
    })?;

    if outcome.accepted {
        info!(
            tx_hash,
            "✓ Worker transaction state updated to Accepted in DA DB"
        );
    } else {
        let status_display = outcome
            .status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "network_error".to_string());
        error!(
            tx_hash,
            status = status_display.as_str(),
            raw_response = ?outcome.raw_response,
            "Sequencer rejected worker transaction: {}",
            outcome.log_message
        );
    }

    Ok(())
}

async fn submit_worker_tx_to_sequencer(
    state: &AppState,
    tx_hash: &str,
) -> Result<SequencerSubmissionOutcome, ServiceError> {
    info!(
        tx_hash,
        node_base_url = %state.node_client.base_url,
        da_connection = %redact_db_connection_string(&state.config.da_connection_string),
        "submit_worker_tx_to_sequencer: starting sequencer submission"
    );

    // First, send the worker transaction to the sequencer and obtain its outcome.
    let outcome = send_worker_tx_to_sequencer(state, tx_hash).await?;

    info!(
        tx_hash,
        accepted = outcome.accepted,
        status_code = ?outcome.status_code,
        latency_ms = format!("{:.2}", outcome.latency_ms),
        internal_ms = ?outcome.internal_ms,
        "submit_worker_tx_to_sequencer: sequencer responded"
    );

    // Apply the DB update in the background so this function's latency is
    // dominated by the sequencer HTTP round-trip, not SQLite/Postgres writes.
    //
    // Errors are logged but do not affect the returned outcome; this mirrors
    // the behavior used in flush_pending_handler.
    let state_clone = state.clone();
    let tx_hash_owned = tx_hash.to_string();
    let outcome_clone = SequencerSubmissionOutcome {
        accepted: outcome.accepted,
        status_code: outcome.status_code,
        raw_response: outcome.raw_response.clone(),
        response_json: outcome.response_json.clone(),
        log_message: outcome.log_message.clone(),
        latency_ms: outcome.latency_ms,
        internal_ms: outcome.internal_ms,
        internal_breakdown: outcome.internal_breakdown.clone(),
    };

    tokio::spawn(async move {
        if let Err(err) =
            update_worker_tx_after_submission(&state_clone, &tx_hash_owned, &outcome_clone).await
        {
            error!(
                tx_hash = %tx_hash_owned,
                "Failed to update worker transaction after sequencer submission in background: {}",
                err
            );
        }
    });

    Ok(outcome)
}

// NOTE: Ligero binary/shader discovery is handled by `ligero-webgpu-runner` inside the
// Sovereign Ligero adapter. This service must not require env vars like `LIGERO_VERIFIER_BIN`
// or `LIGERO_SHADER_PATH` (those binaries are owned by the Ligero repo, not Sovereign).

/// Compute the method ID for the value_validator_rust.wasm program
fn compute_value_setter_method_id() -> Result<[u8; 32]> {
    compute_method_id_for_program("value_validator_rust")
}

/// Compute the method ID for the note_spend_guest.wasm program
fn compute_midnight_method_id() -> Result<[u8; 32]> {
    compute_method_id_for_program("note_spend_guest")
}

/// Generic function to compute method ID for any guest program
fn compute_method_id_for_program(program_name: &str) -> Result<[u8; 32]> {
    // We only pass a circuit name here; `ligero-runner` is responsible for resolving the actual wasm.
    let program_str = program_name.to_string();
    let host = <Ligero as Zkvm>::Host::from_args(&program_str);
    let method_id = host.code_commitment();

    let encoded = method_id.encode();
    let mut result = [0u8; 32];
    result.copy_from_slice(&encoded[..32]);
    Ok(result)
}

fn load_private_key<P: AsRef<Path>>(
    path: P,
) -> Result<<<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey> {
    #[derive(Deserialize)]
    struct PrivateKeyAndAddress<S: Spec> {
        private_key: <S::CryptoSpec as CryptoSpec>::PrivateKey,
        #[allow(dead_code)]
        address: S::Address,
    }

    let data = std::fs::read_to_string(path)?;
    let key_and_address: PrivateKeyAndAddress<RollupSpec> = serde_json::from_str(&data)?;

    Ok(key_and_address.private_key)
}

/// Submit the non-ZK transaction to the rollup node
///
/// This uses the same endpoint as sov-cli: POST /sequencer/txs
async fn submit_to_node(state: &AppState, tx_bytes: Vec<u8>) -> Result<String, ServiceError> {
    debug!("Submitting transaction to node ({} bytes)", tx_bytes.len());

    // Use the node client to submit (same as sov-cli)
    let body = AcceptTxBody {
        body: BASE64_STANDARD.encode(&tx_bytes),
    };

    let response = state
        .node_client
        .client
        .accept_tx(&body)
        .await
        .map_err(|e| ServiceError::SubmissionError(format!("Failed to submit: {}", e)))?;

    let tx_hash = response.id.as_str().to_string();

    debug!("Transaction submitted successfully: {}", tx_hash);

    Ok(tx_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    fn hex_repeat(byte: u8, n: usize) -> String {
        hex::encode(vec![byte; n])
    }

    fn arg_hex32(byte: u8) -> serde_json::Value {
        json!({ "hex": hex_repeat(byte, 32) })
    }

    fn arg_i64(v: i64) -> serde_json::Value {
        json!({ "i64": v })
    }

    fn build_min_note_spend_args_with_viewer(
        withdraw_amount: u64,
        n_out: usize,
        viewer_fvk_commitment_hex: &str,
        pool_sig_hex: Option<&str>,
    ) -> Vec<serde_json::Value> {
        // Minimal, structurally valid args array for note_spend_guest v2 indexing logic:
        // - depth=1, n_in=1
        // - includes blacklist section (1 or 2 checks)
        // - includes viewer section for n_viewers=1
        let depth: i64 = 1;
        let n_in: i64 = 1;

        let mut args: Vec<serde_json::Value> = Vec::new();

        // Header
        args.push(arg_hex32(0x01)); // domain
        args.push(arg_hex32(0x02)); // spend_sk
        args.push(arg_hex32(0x03)); // pk_ivk_owner
        args.push(arg_i64(depth)); // depth
        args.push(arg_hex32(0x04)); // anchor
        args.push(arg_i64(n_in)); // n_in

        // One input (depth=1)
        args.push(arg_i64(1)); // value_in
        args.push(arg_hex32(0x05)); // rho_in
        args.push(arg_hex32(0x06)); // sender_id_in
        args.push(arg_i64(0)); // pos
        args.push(arg_hex32(0x07)); // siblings[0]
        args.push(arg_hex32(0x08)); // nullifier (public)

        // Withdraw binding
        args.push(arg_i64(withdraw_amount as i64)); // withdraw_amount
        args.push(arg_hex32(0x00)); // withdraw_to (ignored by locator)
        args.push(arg_i64(n_out as i64)); // n_out

        // Outputs (5 args each)
        for _ in 0..n_out {
            args.push(arg_i64(1)); // value_out
            args.push(arg_hex32(0x09)); // rho_out
            args.push(arg_hex32(0x0a)); // pk_spend_out
            args.push(arg_hex32(0x0b)); // pk_ivk_out
            args.push(arg_hex32(0x0c)); // cm_out (public)
        }

        // inv_enforce
        args.push(arg_hex32(0x0d));

        // blacklist_root
        args.push(arg_hex32(0x0e));

        // deny-map checks
        let checks: usize = if withdraw_amount == 0 { 2 } else { 1 };
        let bl_bucket_size: usize = midnight_privacy::BLACKLIST_BUCKET_SIZE as usize;
        let bl_depth: usize = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
        for _ in 0..checks {
            for _ in 0..bl_bucket_size {
                args.push(arg_hex32(0x0f)); // bucket_entries[*]
            }
            args.push(arg_hex32(0x10)); // bucket_inv
            for _ in 0..bl_depth {
                args.push(arg_hex32(0x11)); // bucket_siblings[*]
            }
        }

        // Viewer section
        args.push(arg_i64(1)); // n_viewers

        let mut viewer_commit_obj = json!({ "hex": viewer_fvk_commitment_hex });
        if let Some(sig) = pool_sig_hex {
            viewer_commit_obj["pool_sig_hex"] = json!(sig);
        }
        args.push(viewer_commit_obj); // fvk_commitment (public + pool sig metadata)
        args.push(arg_hex32(0x12)); // fvk (private; value irrelevant for locator)

        // ct_hash + mac for each output (public)
        for _ in 0..n_out {
            args.push(arg_hex32(0x13)); // ct_hash
            args.push(arg_hex32(0x14)); // mac
        }

        args
    }

    #[test]
    fn enforce_pool_signed_viewer_commitment_happy_path() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let fvk_commitment: [u8; 32] = [42u8; 32];
        let sig_hex = hex::encode(signing_key.sign(&fvk_commitment).to_bytes());

        let args = build_min_note_spend_args_with_viewer(
            0, // transfer shape
            1,
            &hex::encode(fvk_commitment),
            Some(&sig_hex),
        );

        let package = sov_ligero_adapter::LigeroProofPackage {
            proof: vec![],
            public_output: vec![],
            args_json: serde_json::to_vec(&args).unwrap(),
            private_indices: vec![],
        };
        let proof_bytes = bincode::serialize(&package).unwrap();

        let got = enforce_pool_signed_viewer_commitment(&verifying_key, &proof_bytes).unwrap();
        assert_eq!(got, fvk_commitment);
    }

    #[test]
    fn enforce_pool_signed_viewer_commitment_requires_signature() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let fvk_commitment: [u8; 32] = [42u8; 32];
        let args = build_min_note_spend_args_with_viewer(
            0, // transfer shape
            1,
            &hex::encode(fvk_commitment),
            None,
        );

        let package = sov_ligero_adapter::LigeroProofPackage {
            proof: vec![],
            public_output: vec![],
            args_json: serde_json::to_vec(&args).unwrap(),
            private_indices: vec![],
        };
        let proof_bytes = bincode::serialize(&package).unwrap();

        let err = enforce_pool_signed_viewer_commitment(&verifying_key, &proof_bytes).unwrap_err();
        match err {
            ServiceError::SignatureError(_) => {}
            other => panic!("Expected SignatureError, got {other:?}"),
        }
    }
}
