//! Off-chain Parallel Proof Verification Service
//!
//! This service receives signed Ligero transactions, verifies them in parallel,
//! and transforms them into non-ZK transactions for the rollup node.

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use base64::{prelude::BASE64_STANDARD, Engine};
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use sea_orm::{
    sea_query::OnConflict, ActiveValue::Set, ConnectOptions, Database, DatabaseConnection,
    EntityTrait,
};
use serde::{Deserialize, Serialize};
use sov_api_spec::types::AcceptTxBody;
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment, LigeroVerifier};
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
    zk::{CodeCommitment, CryptoSpec, ZkVerifier, Zkvm, ZkvmHost},
};
use std::{path::{Path, PathBuf}, sync::Arc};
use tracing::{debug, error, info};

// Import the actual demo-stf Runtime types
use demo_stf::runtime::Runtime as DemoRuntime;
use midnight_privacy::{CallMessage as MidnightCallMessage, Hash32 as MidnightHash32, SpendPublic};
use sov_address::MultiAddressEvm;
use sov_midnight_da::storable::{setup_db as setup_midnight_da_db, worker_verified_transactions};
use sov_midnight_da::MidnightDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_stf_blueprint::Runtime as RuntimeTrait;

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
    /// If None, it will be computed from the value_validator.wasm program
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
}

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    config: Arc<ServiceConfig>,
    node_client: NodeClient,
    /// Semaphore to limit concurrent verifications
    verification_semaphore: Arc<tokio::sync::Semaphore>,
    /// Local nonce counter (synchronized across all requests)
    nonce_counter: Arc<tokio::sync::Mutex<Option<u64>>>,
    /// Cached signing key (loaded once at startup)
    signing_key: Arc<<<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey>,
    /// Connection to the MockDA database shared with the rollup node
    da_conn: Arc<DatabaseConnection>,
}

impl AppState {
    pub async fn new(mut config: ServiceConfig) -> Result<Self, anyhow::Error> {
        let max_permits = config.max_concurrent_verifications;
        let node_client = NodeClient::new_unchecked(&config.node_rpc_url);

        // Load signing key once at startup
        let signing_key = load_private_key(&config.signing_key_path)
            .context("Failed to load signing key at startup")?;

        info!("✓ Loaded signing key from: {}", config.signing_key_path);

        // Compute value-setter method ID if not provided
        if config.value_setter_method_id.is_none() {
            info!("Computing value-setter method ID from value_validator.wasm...");
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

        let mut connect_opts = ConnectOptions::new(config.da_connection_string.clone());
        connect_opts.max_connections(20).sqlx_logging(false);
        let da_conn = Database::connect(connect_opts).await.with_context(|| {
            format!(
                "Failed to connect to MockDA database at {}",
                config.da_connection_string
            )
        })?;
        setup_midnight_da_db(&da_conn)
            .await
            .context("Failed to initialize MockDA database schema")?;
        info!("✓ Connected to MockDA database");

        Ok(Self {
            config: Arc::new(config),
            node_client,
            verification_semaphore: Arc::new(tokio::sync::Semaphore::new(max_permits)),
            nonce_counter: Arc::new(tokio::sync::Mutex::new(None)),
            signing_key: Arc::new(signing_key),
            da_conn: Arc::new(da_conn),
        })
    }
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
        .route("/midnight-privacy", post(verify_and_record_midnight_handler))
        .route("/health", axum::routing::get(health_check))
        .with_state(state)
        // Remove default 2MB body limit and set 10MB for large Ligero proofs (~3.2MB each)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(
                    tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO),
                )
                .on_response(
                    tower_http::trace::DefaultOnResponse::new().level(tracing::Level::INFO),
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

/// Main handler for verify-and-submit endpoint
async fn verify_and_submit_handler(
    State(state): State<AppState>,
    Json(req): Json<VerifyAndSubmitRequest>,
) -> Result<Json<VerifyAndSubmitResponse>, ServiceError> {
    let start = std::time::Instant::now();
    let mut metrics = VerificationMetrics::default();

    info!("Received verification request");

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

    info!("✓ Proof verification successful for value={}", value);

    // Step 4: Create and sign non-ZK transaction
    let tx_start = std::time::Instant::now();
    let signed_tx_bytes = create_and_sign_non_zk_transaction(&state, value).await?;
    metrics.tx_creation_ms = tx_start.elapsed().as_secs_f64() * 1000.0;

    // Step 5: Submit to node
    let submit_start = std::time::Instant::now();
    let tx_hash = submit_to_node(&state, signed_tx_bytes).await?;
    metrics.node_submit_ms = submit_start.elapsed().as_secs_f64() * 1000.0;

    metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

    info!(
        "✓ Successfully processed transaction: value={}, hash={}, total_time={:.2}ms",
        value, tx_hash, metrics.total_ms
    );

    Ok(Json(VerifyAndSubmitResponse {
        success: true,
        tx_hash: Some(tx_hash),
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

    info!("Received midnight transaction verification request");

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
            ServiceError::ParseError(format!("Failed to deserialize transaction: {}", e))
        })?;
    
    let parsed_call = parse_midnight_call(&tx)?;
    metrics.parse_ms = parse_start.elapsed().as_secs_f64() * 1000.0;

    let signature_start = std::time::Instant::now();
    verify_midnight_transaction_signature(&tx)?;
    metrics.signature_verify_ms = signature_start.elapsed().as_secs_f64() * 1000.0;

    let tx_hash = tx.hash().to_string();
    let transaction_data = create_transaction_without_proof(&tx)?;

    match parsed_call {
        ParsedMidnightCall::Deposit { amount, rho, recipient } => {
            // Deposits don't have proofs, so we just verify signature and store
            debug!(
                "Parsed midnight deposit: amount={}, rho=0x{}, recipient=0x{}",
                amount,
                hex::encode(&rho[..8]),
                hex::encode(&recipient[..8])
            );

            let persist_start = std::time::Instant::now();
            store_verified_midnight_transaction(
                state.da_conn.as_ref(),
                &tx_hash,
                None, // No proof outputs for deposits
                true, // signature_valid
                None, // proof_verified: NULL (transaction doesn't have a proof)
                &transaction_data,
                &req.body,
            )
            .await?;
            metrics.tx_creation_ms = persist_start.elapsed().as_secs_f64() * 1000.0;
            metrics.proof_verify_ms = 0.0;
            metrics.node_submit_ms = 0.0;
            metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

            info!(
                "✓ Stored verified midnight deposit: amount={}, rho=0x{}, hash={}, total_time={:.2}ms",
                amount,
                hex::encode(&rho[..8]),
                tx_hash,
                metrics.total_ms
            );

            Ok(Json(VerifyAndSubmitResponse {
                success: true,
                tx_hash: Some(tx_hash),
                error: None,
                metrics,
            }))
        }
        ParsedMidnightCall::Withdraw {
            proof,
            anchor_root,
            nullifier,
            withdraw_amount,
            to,
        } => {
            // Withdrawals have proofs that need verification
            debug!(
                "Parsed midnight withdrawal: nullifier=0x{}, anchor_root=0x{}, withdraw_amount={}, proof_size={} bytes",
                hex::encode(nullifier),
                hex::encode(anchor_root),
                withdraw_amount,
                proof.len()
            );

            let proof_start = std::time::Instant::now();
            let proof_public = verify_midnight_withdraw_proof(
                state.config.midnight_method_id.as_ref(),
                &proof,
                anchor_root,
                nullifier,
                withdraw_amount,
            )
            .await?;
            metrics.proof_verify_ms = proof_start.elapsed().as_secs_f64() * 1000.0;

            let persist_start = std::time::Instant::now();
            store_verified_midnight_transaction(
                state.da_conn.as_ref(),
                &tx_hash,
                Some(&proof_public), // Proof outputs from verification
                true,                 // signature_valid
                Some(true),          // proof_verified: true (has proof and verified correctly)
                &transaction_data,
                &req.body,
            )
            .await?;
            metrics.tx_creation_ms = persist_start.elapsed().as_secs_f64() * 1000.0;
            metrics.node_submit_ms = 0.0;
            metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;

            info!(
                "✓ Stored verified midnight withdrawal: nullifier=0x{}, anchor_root=0x{}, withdraw_amount={}, to={:?}, hash={}, total_time={:.2}ms",
                hex::encode(nullifier),
                hex::encode(anchor_root),
                withdraw_amount,
                to,
                tx_hash,
                metrics.total_ms
            );

            Ok(Json(VerifyAndSubmitResponse {
                success: true,
                tx_hash: Some(tx_hash),
                error: None,
                metrics,
            }))
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

/// Verify Ligero proof using the LigeroVerifier (same as on-chain verification)
async fn verify_ligero_proof(
    state: &AppState,
    value: u32,
    proof: &[u8],
) -> Result<(), ServiceError> {
    let method_id_bytes = state.config.value_setter_method_id.ok_or_else(|| {
        ServiceError::Internal(
            "Value-setter method ID not configured. \
            The service needs the value_validator.wasm program to compute the method ID."
                .to_string(),
        )
    })?;
    let method_id = LigeroCodeCommitment(method_id_bytes);

    // Spawn blocking task for CPU-intensive proof verification
    let proof = proof.to_vec();
    let result = tokio::task::spawn_blocking(move || {
        // Set environment variables for value_validator.wasm verification
        configure_ligero_env_for_value_setter()?;
        
        let package: sov_ligero_adapter::LigeroProofPackage = bincode::deserialize(&proof)
            .map_err(|err| {
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

        // Use LigeroVerifier to verify the proof (same as module-level verification)
        let public: ValueProofPublic = LigeroVerifier::verify(&proof, &method_id)
            .map_err(|e| ServiceError::ProofError(format!("Verification failed: {}", e)))?;

        // Check that the public output matches the claimed value
        if public.value != value {
            return Err(ServiceError::ProofError(format!(
                "Value mismatch: claimed={}, verified={}",
                value, public.value
            )));
        }

        Ok(())
    })
    .await
    .map_err(|e| ServiceError::Internal(format!("Task join error: {}", e)))?;

    result
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
        "  Using Runtime's CHAIN_HASH: {}",
        hex::encode(&DemoRuntime::<RollupSpec>::CHAIN_HASH)
    );

    // Create the transaction
    // Note: This is a simplified version. In production, you'd use the actual
    // Runtime::Call type and properly construct the transaction.

    // For now, we'll use a generic approach that creates a transaction structure
    // that matches what sov-cli would create

    let signed_tx_bytes =
        create_value_setter_tx_bytes(value, nonce, signing_key, state.config.chain_id).map_err(
            |e| {
                error!("Failed to create transaction bytes: {}", e);
                ServiceError::Internal(format!("Failed to create transaction: {}", e))
            },
        )?;

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
    debug!(
        "CHAIN_HASH for signing: 0x{}",
        hex::encode(&<DemoRuntime<RollupSpec> as RuntimeTrait<RollupSpec>>::CHAIN_HASH)
    );

    // Sign the transaction using the Runtime's CHAIN_HASH (same as sov-cli does)
    let signed_tx = Transaction::<DemoRuntime<RollupSpec>, RollupSpec>::new_signed_tx(
        signing_key,
        &<DemoRuntime<RollupSpec> as RuntimeTrait<RollupSpec>>::CHAIN_HASH,
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
    },
    Withdraw {
        proof: Vec<u8>,
        anchor_root: MidnightHash32,
        nullifier: MidnightHash32,
        withdraw_amount: u128,
        to: <RollupSpec as Spec>::Address,
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
                ..
            } => Ok(ParsedMidnightCall::Deposit {
                amount,
                rho,
                recipient,
            }),
            MidnightCallMessage::Withdraw {
                proof,
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
                ..
            } => Ok(ParsedMidnightCall::Withdraw {
                proof: proof.into(),
                anchor_root,
                nullifier,
                withdraw_amount,
                to,
            }),
            other => Err(ServiceError::UnsupportedCall(format!(
                "Unsupported midnight_privacy call: {other:?}"
            ))),
        },
        other => Err(ServiceError::UnsupportedCall(format!(
            "Expected midnight_privacy call, got {other:?}"
        ))),
    }
}

fn parse_midnight_withdraw_call(
    tx: &Transaction<DemoRuntime<RollupSpec>, RollupSpec>,
) -> Result<
    (
        Vec<u8>,
        MidnightHash32,
        MidnightHash32,
        u128,
        <RollupSpec as Spec>::Address,
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
        } => Ok((proof, anchor_root, nullifier, withdraw_amount, to)),
        ParsedMidnightCall::Deposit { .. } => Err(ServiceError::UnsupportedCall(
            "Expected Withdraw call, got Deposit".to_string(),
        )),
    }
}

fn verify_midnight_transaction_signature(
    tx: &Transaction<DemoRuntime<RollupSpec>, RollupSpec>,
) -> Result<(), ServiceError> {
    let mut meter = UnlimitedGasMeter::<RollupSpec>::default();
    tx.verify(&DemoRuntime::<RollupSpec>::CHAIN_HASH, &mut meter)
        .map_err(|e| ServiceError::SignatureError(e.to_string()))
}

async fn verify_midnight_withdraw_proof(
    method_id_opt: Option<&[u8; 32]>,
    proof: &[u8],
    expected_anchor_root: MidnightHash32,
    expected_nullifier: MidnightHash32,
    expected_withdraw_amount: u128,
) -> Result<SpendPublic, ServiceError> {
    let method_id_bytes = method_id_opt.ok_or_else(|| {
        ServiceError::Internal(
            "Midnight method ID not configured. \
            The service needs the note_spend_guest.wasm program to compute the method ID."
                .to_string(),
        )
    })?;
    let method_id = LigeroCodeCommitment(*method_id_bytes);
    let proof_vec = proof.to_vec();

    tokio::task::spawn_blocking(move || {
        // Set environment variables for note_spend_guest.wasm verification
        configure_ligero_env_for_midnight()?;
        
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

        let public: SpendPublic = LigeroVerifier::verify(&proof_vec, &method_id)
            .map_err(|e| ServiceError::ProofError(format!("Verification failed: {}", e)))?;

        if public.anchor_root != expected_anchor_root {
            return Err(ServiceError::ProofError(format!(
                "Anchor root mismatch: expected 0x{}, proof 0x{}",
                hex::encode(expected_anchor_root),
                hex::encode(public.anchor_root)
            )));
        }
        if public.nullifier != expected_nullifier {
            return Err(ServiceError::ProofError(format!(
                "Nullifier mismatch: expected 0x{}, proof 0x{}",
                hex::encode(expected_nullifier),
                hex::encode(public.nullifier)
            )));
        }
        if public.withdraw_amount != expected_withdraw_amount {
            return Err(ServiceError::ProofError(format!(
                "Withdraw amount mismatch: expected {}, proof {}",
                expected_withdraw_amount, public.withdraw_amount
            )));
        }

        Ok(public)
    })
    .await
    .map_err(|e| ServiceError::Internal(format!("Task join error: {}", e)))?
}

/// Create a transaction JSON representation without the proof data
/// Extracts the runtime call message and replaces proof with "REMOVED"
fn create_transaction_without_proof(
    tx: &DemoTransaction,
) -> Result<String, ServiceError> {
    // Extract the runtime call and create JSON representation with proof removed
    match tx.runtime_call() {
        RuntimeCall::MidnightPrivacy(call) => {
            let call_json = match call.clone() {
                MidnightCallMessage::Deposit { amount, rho, recipient, gas } => {
                    serde_json::json!({
                        "deposit": {
                            "amount": amount.to_string(),
                            "rho": hex::encode(rho),
                            "recipient": format!("{:?}", recipient),
                            "gas": gas
                        }
                    })
                }
                MidnightCallMessage::Transfer {
                    anchor_root,
                    nullifier,
                    gas,
                    ..
                } => {
                    serde_json::json!({
                        "transfer": {
                            "proof": "REMOVED",
                            "anchor_root": hex::encode(anchor_root),
                            "nullifier": hex::encode(nullifier),
                            "gas": gas
                        }
                    })
                }
                MidnightCallMessage::Withdraw {
                    anchor_root,
                    nullifier,
                    withdraw_amount,
                    to,
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
                ValueSetterCallMessage::AssertVisibleSlotNumber { expected_visible_slot_number } => {
                    serde_json::json!({
                        "assert_visible_slot_number": {
                            "expected_visible_slot_number": expected_visible_slot_number
                        }
                    })
                }
                ValueSetterCallMessage::SetValueAndSleep { value, sleep_millis } => {
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

async fn store_verified_midnight_transaction(
    conn: &DatabaseConnection,
    tx_hash: &str,
    proof_output: Option<&SpendPublic>,
    signature_valid: bool,
    proof_verified: Option<bool>,
    transaction_data: &str,
    full_transaction_blob: &str,
) -> Result<(), ServiceError> {
    use worker_verified_transactions::{
        ActiveModel as VerifiedActiveModel, Column as VerifiedColumn, Entity as VerifiedEntity,
        TransactionState,
    };

    // Serialize proof outputs to JSON (empty object if no proof)
    let proof_outputs_json = if let Some(proof) = proof_output {
        serde_json::to_string(proof).map_err(|err| {
            ServiceError::Internal(format!("Failed to serialize proof outputs: {err}"))
        })?
    } else {
        "{}".to_string()
    };

    VerifiedEntity::insert(VerifiedActiveModel {
        tx_hash: Set(tx_hash.to_owned()),
        signature_valid: Set(signature_valid),
        proof_verified: Set(proof_verified),
        transaction_data: Set(transaction_data.to_owned()),
        full_transaction_blob: Set(full_transaction_blob.to_owned()),
        proof_outputs: Set(proof_outputs_json),
        transaction_state: Set(TransactionState::Pending),
        sequencer_status: Set(None),
        created_at: Set(Utc::now()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(VerifiedColumn::TxHash)
            .update_columns([
                VerifiedColumn::SignatureValid,
                VerifiedColumn::ProofVerified,
                VerifiedColumn::TransactionData,
                VerifiedColumn::FullTransactionBlob,
                VerifiedColumn::ProofOutputs,
                VerifiedColumn::TransactionState,
                VerifiedColumn::SequencerStatus,
                VerifiedColumn::CreatedAt,
            ])
            .to_owned(),
    )
    .exec(conn)
    .await
    .map_err(|err| {
        ServiceError::Internal(format!("Failed to store verified transaction: {err}"))
    })?;

    Ok(())
}

/// Configure Ligero environment variables for value-setter verification
fn configure_ligero_env_for_value_setter() -> Result<(), ServiceError> {
    let current_dir = std::env::current_dir()
        .map_err(|e| ServiceError::Internal(format!("Failed to get current directory: {}", e)))?;
    
    let program_paths = vec![
        current_dir.join("crates/adapters/ligero/guest/bins/programs/value_validator.wasm"),
        current_dir.join("../crates/adapters/ligero/guest/bins/programs/value_validator.wasm"),
        current_dir.join("../../crates/adapters/ligero/guest/bins/programs/value_validator.wasm"),
    ];
    
    let program_path = program_paths
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| {
            ServiceError::Internal(format!(
                "Could not find value_validator.wasm. Searched:\n{}",
                program_paths.iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        })?;
    
    // Find shader and verifier binary paths
    let shader_path = find_ligero_shader_path(&current_dir)?;
    let verifier_bin = find_ligero_verifier_bin(&current_dir)?;
    
    std::env::set_var("LIGERO_PROGRAM_PATH", program_path);
    std::env::set_var("LIGERO_SHADER_PATH", shader_path);
    std::env::set_var("LIGERO_VERIFIER_BIN", verifier_bin);
    std::env::set_var("LIGERO_PACKING", "8192");
    
    Ok(())
}

/// Configure Ligero environment variables for midnight verification
fn configure_ligero_env_for_midnight() -> Result<(), ServiceError> {
    let current_dir = std::env::current_dir()
        .map_err(|e| ServiceError::Internal(format!("Failed to get current directory: {}", e)))?;
    
    let program_paths = vec![
        current_dir.join("crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"),
        current_dir.join("../crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"),
        current_dir.join("../../crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"),
    ];
    
    let program_path = program_paths
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| {
            ServiceError::Internal(format!(
                "Could not find note_spend_guest.wasm. Searched:\n{}",
                program_paths.iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        })?;
    
    // Find shader and verifier binary paths
    let shader_path = find_ligero_shader_path(&current_dir)?;
    let verifier_bin = find_ligero_verifier_bin(&current_dir)?;
    
    std::env::set_var("LIGERO_PROGRAM_PATH", program_path);
    std::env::set_var("LIGERO_SHADER_PATH", shader_path);
    std::env::set_var("LIGERO_VERIFIER_BIN", verifier_bin);
    std::env::set_var("LIGERO_PACKING", "8192");
    
    Ok(())
}

/// Find the Ligero shader path
fn find_ligero_shader_path(current_dir: &Path) -> Result<PathBuf, ServiceError> {
    let candidates = vec![
        current_dir.join("crates/adapters/ligero/bins/macos/shader"),
        current_dir.join("../crates/adapters/ligero/bins/macos/shader"),
        current_dir.join("../../crates/adapters/ligero/bins/macos/shader"),
        current_dir.join("crates/adapters/ligero/bins/linux-amd64/shader"),
    ];
    
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .ok_or_else(|| {
            ServiceError::Internal(format!(
                "Could not find Ligero shader directory. Searched:\n{}",
                candidates.iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        })
}

/// Find the Ligero verifier binary
fn find_ligero_verifier_bin(current_dir: &Path) -> Result<PathBuf, ServiceError> {
    let candidates = vec![
        current_dir.join("crates/adapters/ligero/bins/macos/bin/webgpu_verifier"),
        current_dir.join("../crates/adapters/ligero/bins/macos/bin/webgpu_verifier"),
        current_dir.join("../../crates/adapters/ligero/bins/macos/bin/webgpu_verifier"),
        current_dir.join("crates/adapters/ligero/bins/linux-amd64/bin/webgpu_verifier"),
    ];
    
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .ok_or_else(|| {
            ServiceError::Internal(format!(
                "Could not find Ligero verifier binary. Searched:\n{}",
                candidates.iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        })
}

/// Compute the method ID for the value_validator.wasm program
fn compute_value_setter_method_id() -> Result<[u8; 32]> {
    compute_method_id_for_program("value_validator.wasm")
}

/// Compute the method ID for the note_spend_guest.wasm program
fn compute_midnight_method_id() -> Result<[u8; 32]> {
    compute_method_id_for_program("note_spend_guest.wasm")
}

/// Generic function to compute method ID for any guest program
fn compute_method_id_for_program(program_name: &str) -> Result<[u8; 32]> {
    let current_dir = std::env::current_dir()?;
    
    // Try multiple possible locations
    let possible_paths = vec![
        current_dir.join(format!("crates/adapters/ligero/guest/bins/programs/{}", program_name)),
        current_dir.join(format!("../crates/adapters/ligero/guest/bins/programs/{}", program_name)),
        current_dir.join(format!("../../crates/adapters/ligero/guest/bins/programs/{}", program_name)),
    ];
    
    let program_path = possible_paths
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find {}. Searched:\n{}",
                program_name,
                possible_paths
                    .iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })?;
    
    let program_str = program_path.to_string_lossy().to_string();
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

    use sea_orm::{ConnectOptions, Database, EntityTrait};
    use sov_address::EthereumAddress;
    use sov_modules_api::transaction::VersionedTx;
    use sov_modules_api::SafeVec;
    use std::str::FromStr;

    fn sample_midnight_withdraw_transaction(
        nonce: u64,
    ) -> Transaction<DemoRuntime<RollupSpec>, RollupSpec> {
        let signing_key = <<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey::generate();
        let anchor_root = [42u8; 32];
        let nullifier = [7u8; 32];
        let withdraw_amount = 123_456u128;
        let to = MultiAddressEvm::Vm(
            EthereumAddress::from_str("0x71334bf1710D12c9f689cC819476fA589F08C64C").unwrap(),
        );
        let proof: SafeVec<u8, 5_000_000> =
            SafeVec::try_from(vec![9u8; 16]).expect("within SafeVec capacity");

        let call = MidnightCallMessage::<RollupSpec>::Withdraw {
            proof,
            anchor_root,
            nullifier,
            withdraw_amount,
            to,
            gas: None,
        };

        let details = TxDetails {
            max_fee: Amount::from(100_000_000_000u128),
            max_priority_fee_bips: PriorityFeeBips(0),
            gas_limit: None,
            chain_id: 4321,
        };

        let runtime_call = RuntimeCall::MidnightPrivacy(call);
        let unsigned_tx = UnsignedTransaction::new_with_details(
            runtime_call,
            UniquenessData::Generation(nonce),
            details,
        );

        Transaction::<DemoRuntime<RollupSpec>, RollupSpec>::new_signed_tx(
            &signing_key,
            &<DemoRuntime<RollupSpec> as RuntimeTrait<RollupSpec>>::CHAIN_HASH,
            unsigned_tx,
        )
    }

    #[test]
    fn test_value_setter_call_serialization() {
        let call = ValueSetterCall::SetValue {
            value: 42,
            gas: None,
        };

        let bytes = borsh::to_vec(&call).unwrap();
        assert!(!bytes.is_empty());

        let deserialized: ValueSetterCall = borsh::from_slice(&bytes).unwrap();
        match deserialized {
            ValueSetterCall::SetValue { value, .. } => assert_eq!(value, 42),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_parse_midnight_withdraw_call_roundtrips() {
        let tx = sample_midnight_withdraw_transaction(0);
        let (proof_bytes, anchor_root, nullifier, withdraw_amount, recipient) =
            parse_midnight_withdraw_call(&tx).expect("parse succeeds");

        assert_eq!(proof_bytes.len(), 16);
        assert_eq!(anchor_root, [42u8; 32]);
        assert_eq!(nullifier, [7u8; 32]);
        assert_eq!(withdraw_amount, 123_456u128);
        assert_eq!(
            recipient,
            MultiAddressEvm::Vm(
                EthereumAddress::from_str("0x71334bf1710D12c9f689cC819476fA589F08C64C").unwrap()
            )
        );
    }

    #[test]
    fn test_verify_midnight_transaction_signature_accepts_valid() {
        let tx = sample_midnight_withdraw_transaction(5);
        verify_midnight_transaction_signature(&tx).expect("signature valid");
    }

    #[test]
    fn test_verify_midnight_transaction_signature_rejects_tampered() {
        let mut tx = sample_midnight_withdraw_transaction(7);
        let VersionedTx::V0(inner) = &mut tx.versioned_tx;
        if let RuntimeCall::MidnightPrivacy(MidnightCallMessage::Withdraw {
            ref mut withdraw_amount,
            ..
        }) = inner.runtime_call
        {
            *withdraw_amount += 1;
        }

        match verify_midnight_transaction_signature(&tx) {
            Err(ServiceError::SignatureError(_)) => {}
            other => panic!("expected signature error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_verified_midnight_transaction_upsert() {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_string());
        opts.max_connections(1).sqlx_logging(false);
        let conn = Database::connect(opts).await.unwrap();
        setup_midnight_da_db(&conn).await.unwrap();

        let tx = sample_midnight_withdraw_transaction(9);
        let tx_hash = tx.hash().to_string();
        let tx_json = serde_json::to_string(&tx).unwrap();
        let full_blob = "base64encodedtransaction";
        let mut proof_public = SpendPublic {
            anchor_root: [1u8; 32],
            nullifier: [2u8; 32],
            withdraw_amount: 55,
            output_commitments: vec![],
        };

        store_verified_midnight_transaction(&conn, &tx_hash, Some(&proof_public), true, Some(true), &tx_json, full_blob)
            .await
            .unwrap();

        proof_public.withdraw_amount = 99;
        store_verified_midnight_transaction(&conn, &tx_hash, Some(&proof_public), true, Some(true), &tx_json, full_blob)
            .await
            .unwrap();

        let stored = worker_verified_transactions::Entity::find()
            .all(&conn)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        let record = &stored[0];
        assert_eq!(record.tx_hash, tx_hash);
        assert!(record.signature_valid);
        assert_eq!(record.proof_verified, Some(true));
        
        // Verify proof outputs are stored as JSON
        let proof_outputs: SpendPublic = serde_json::from_str(&record.proof_outputs).unwrap();
        assert_eq!(proof_outputs.withdraw_amount, proof_public.withdraw_amount);
        assert_eq!(proof_outputs.anchor_root, proof_public.anchor_root);
        assert_eq!(proof_outputs.nullifier, proof_public.nullifier);
        assert_eq!(record.transaction_data, tx_json);
    }

    #[tokio::test]
    async fn test_store_deposit_transaction_without_proof() {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_string());
        opts.max_connections(1).sqlx_logging(false);
        let conn = Database::connect(opts).await.unwrap();
        setup_midnight_da_db(&conn).await.unwrap();

        let tx_hash = "0xdeposit123";
        let transaction_data = r#"{"deposit":{"amount":"100","rho":"0x01...","recipient":"0x02...","gas":null}}"#;
        let full_blob = "base64encodeddeposittransaction";

        // Store deposit with no proof
        store_verified_midnight_transaction(
            &conn,
            tx_hash,
            None,       // No proof outputs for deposits
            true,       // signature_valid
            None,       // proof_verified: NULL (transaction doesn't have a proof)
            transaction_data,
            full_blob,
        )
        .await
        .unwrap();

        let stored = worker_verified_transactions::Entity::find()
            .all(&conn)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        
        let record = &stored[0];
        assert_eq!(record.tx_hash, tx_hash);
        assert!(record.signature_valid, "Signature should be valid");
        assert_eq!(record.proof_verified, None, "proof_verified should be NULL for deposits");
        assert_eq!(record.proof_outputs, "{}", "proof_outputs should be empty JSON for deposits");
        assert_eq!(record.transaction_data, transaction_data);
        assert_eq!(record.full_transaction_blob, full_blob);
    }

    #[tokio::test]
    async fn test_verify_midnight_withdraw_proof_invalid_payload() {
        let proof = vec![0u8; 4];
        let anchor_root = [3u8; 32];
        let nullifier = [4u8; 32];
        let withdraw_amount = 77u128;

        match verify_midnight_withdraw_proof(
            Some([0u8; 32]).as_ref(),
            &proof,
            anchor_root,
            nullifier,
            withdraw_amount,
        )
        .await
        {
            // Either ProofError (if WASM file exists) or Internal error (if WASM file not found)
            Err(ServiceError::ProofError(_)) => {
                // Expected: proof is invalid
            }
            Err(ServiceError::Internal(msg)) if msg.contains("note_spend_guest.wasm") => {
                // Also acceptable in test environment: WASM file not found
                // This means we can't even attempt proof verification
            }
            other => panic!("expected proof error or missing WASM file, got {other:?}"),
        }
    }

    /// End-to-end test that mimics the generate_and_send_midnight_tx.sh script
    /// 
    /// This test validates the core workflow:
    /// 1. Transaction creation and serialization (like midnight-tx-generator)
    /// 2. Signature verification
    /// 3. Proof verification (mocked, since we don't have Ligero in test env)
    /// 4. Database storage
    /// 5. Database content verification
    #[tokio::test]
    async fn test_end_to_end_midnight_withdrawal_flow() {
        println!("\n=== Midnight Withdrawal E2E Test ===\n");
        
        // Step 1: Setup database (mimics shared MockDA database)
        let mut db_opts = ConnectOptions::new("sqlite::memory:".to_string());
        db_opts.max_connections(10).sqlx_logging(false);
        let conn = Database::connect(db_opts).await.unwrap();
        setup_midnight_da_db(&conn).await.unwrap();
        println!("✓ Step 1: Database initialized");

        // Step 2: Generate a midnight withdrawal transaction (like midnight-tx-generator does)
        println!("\n✓ Step 2: Creating transaction (like midnight-tx-generator)");
        
        let anchor_root = [0u8; 32]; // All zeros like the script default
        let nullifier = [0u8; 32];
        let withdraw_amount = 500u128; // Matches script default
        let recipient = MultiAddressEvm::Vm(
            EthereumAddress::from_str("0x71334bf1710D12c9f689cC819476fA589F08C64C").unwrap(),
        );
        
        // Use a small dummy proof (real proof would be ~3.2MB from Ligero)
        let dummy_proof: SafeVec<u8, 5_000_000> =
            SafeVec::try_from(vec![0u8; 32]).expect("within SafeVec capacity");

        let signing_key = <<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey::generate();
        
        let call = MidnightCallMessage::<RollupSpec>::Withdraw {
            proof: dummy_proof,
            anchor_root,
            nullifier,
            withdraw_amount,
            to: recipient.clone(),
            gas: None,
        };

        let details = TxDetails {
            max_fee: Amount::from(100_000_000_000u128),
            max_priority_fee_bips: PriorityFeeBips(0),
            gas_limit: None,
            chain_id: 4321,
        };

        let runtime_call = RuntimeCall::MidnightPrivacy(call);
        let unsigned_tx = UnsignedTransaction::new_with_details(
            runtime_call,
            UniquenessData::Generation(0),
            details,
        );

        let tx = Transaction::<DemoRuntime<RollupSpec>, RollupSpec>::new_signed_tx(
            &signing_key,
            &<DemoRuntime<RollupSpec> as RuntimeTrait<RollupSpec>>::CHAIN_HASH,
            unsigned_tx,
        );

        let tx_hash = tx.hash().to_string();
        println!("  Transaction Hash: {}", tx_hash);
        println!("  Anchor Root: 0x{}", hex::encode(anchor_root));
        println!("  Nullifier: 0x{}", hex::encode(nullifier));
        println!("  Withdraw Amount: {}", withdraw_amount);
        println!("  Recipient: {:?}", recipient);

        // Step 3: Serialize to bytes and encode as base64 (like the shell script does)
        let tx_bytes = borsh::to_vec(&tx).expect("Failed to serialize transaction");
        let tx_base64 = BASE64_STANDARD.encode(&tx_bytes);
        println!("\n✓ Step 3: Transaction serialized");
        println!("  Binary: {} bytes", tx_bytes.len());
        println!("  Base64: {} chars", tx_base64.len());

        // Step 4: SIGNATURE VERIFICATION (key test!)
        println!("\n✓ Step 4: Testing signature verification");
        verify_midnight_transaction_signature(&tx)
            .expect("Signature should be valid");
        println!("  ✓ Signature verified successfully!");

        // Step 5: Parse the transaction (extract proof, anchor_root, nullifier, etc.)
        println!("\n✓ Step 5: Testing transaction parsing");
        let (proof_bytes, parsed_anchor_root, parsed_nullifier, parsed_amount, parsed_recipient) =
            parse_midnight_withdraw_call(&tx).expect("Should parse transaction");
        
        assert_eq!(proof_bytes.len(), 32, "Proof should be our dummy 32 bytes");
        assert_eq!(parsed_anchor_root, anchor_root, "Anchor root should match");
        assert_eq!(parsed_nullifier, nullifier, "Nullifier should match");
        assert_eq!(parsed_amount, withdraw_amount, "Withdraw amount should match");
        assert_eq!(parsed_recipient, recipient, "Recipient should match");
        println!("  ✓ All fields parsed correctly!");

        // Step 6: PROOF VERIFICATION (mocked)
        // In production, this would call verify_midnight_withdraw_proof with real Ligero
        // For this test, we simulate the proof output that would be returned
        println!("\n✓ Step 6: Simulating proof verification");
        let simulated_proof_output = SpendPublic {
            anchor_root,
            nullifier,
            withdraw_amount,
            output_commitments: vec![],
        };
        println!("  ✓ Proof verification simulated (would verify with Ligero in production)");
        
        // Step 7: DATABASE STORAGE (key test!)
        println!("\n✓ Step 7: Testing database storage");
        let transaction_data = create_transaction_without_proof(&tx)
            .expect("Should create transaction data");
        
        store_verified_midnight_transaction(
            &conn,
            &tx_hash,
            Some(&simulated_proof_output),
            true,          // signature_valid
            Some(true),    // proof_verified (has proof and verified)
            &transaction_data,
            &tx_base64, // full transaction blob
        )
        .await
        .expect("Should store to database");
        println!("  ✓ Transaction stored to database!");

        // Step 8: DATABASE VERIFICATION (key test!)
        println!("\n✓ Step 8: Verifying database contents");
        let stored = worker_verified_transactions::Entity::find()
            .all(&conn)
            .await
            .unwrap();
        
        assert_eq!(stored.len(), 1, "Should have exactly 1 record");
        let record = &stored[0];
        
        // Verify all fields
        assert_eq!(record.tx_hash, tx_hash, "Transaction hash should match");
        assert!(record.signature_valid, "Signature should be marked valid");
        assert_eq!(record.proof_verified, Some(true), "Proof should be marked verified");
        assert_eq!(record.full_transaction_blob, tx_base64, "Full blob should match");
        
        // Verify proof outputs stored as JSON
        let stored_proof_output: SpendPublic = serde_json::from_str(&record.proof_outputs)
            .expect("Should deserialize proof outputs");
        assert_eq!(stored_proof_output.anchor_root, anchor_root, "Stored anchor root should match");
        assert_eq!(stored_proof_output.nullifier, nullifier, "Stored nullifier should match");
        assert_eq!(stored_proof_output.withdraw_amount, withdraw_amount, "Stored amount should match");
        
        // Verify transaction data (without proof)
        assert!(record.transaction_data.contains("withdraw"), "Should contain withdraw call");
        assert!(record.transaction_data.contains("REMOVED"), "Proof should be removed from data");
        
        // Verify transaction state
        assert_eq!(
            record.transaction_state,
            worker_verified_transactions::TransactionState::Pending,
            "State should be Pending"
        );
        
        println!("  ✓ All database fields verified correctly!");
        
        // Step 9: Test idempotency - storing the same transaction again should update, not duplicate
        println!("\n✓ Step 9: Testing database upsert (idempotency)");
        let mut updated_proof_output = simulated_proof_output.clone();
        updated_proof_output.withdraw_amount = 999; // Change amount
        
        store_verified_midnight_transaction(
            &conn,
            &tx_hash,
            Some(&updated_proof_output),
            true,
            Some(true),
            &transaction_data,
            &tx_base64,
        )
        .await
        .expect("Should update existing record");
        
        let stored_after_update = worker_verified_transactions::Entity::find()
            .all(&conn)
            .await
            .unwrap();
        
        assert_eq!(stored_after_update.len(), 1, "Should still have exactly 1 record (not duplicated)");
        let updated_record = &stored_after_update[0];
        let updated_stored_proof: SpendPublic = serde_json::from_str(&updated_record.proof_outputs).unwrap();
        assert_eq!(updated_stored_proof.withdraw_amount, 999, "Amount should be updated");
        println!("  ✓ Upsert works correctly - no duplicates!");

        println!("\n=== ✓ ALL TESTS PASSED ===");
        println!("\nValidated:");
        println!("  ✓ Transaction creation and serialization");
        println!("  ✓ Signature verification");
        println!("  ✓ Transaction parsing");
        println!("  ✓ Proof verification (simulated)");
        println!("  ✓ Database storage");
        println!("  ✓ Database content verification");
        println!("  ✓ Database upsert/idempotency");
        println!();
    }
}
