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
use serde::{Deserialize, Serialize};
use sov_api_spec::types::AcceptTxBody;
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment, LigeroVerifier};
use sov_modules_api::{
    capabilities::UniquenessData,
    configurable_spec::ConfigurableSpec,
    execution_mode::Native,
    transaction::{PriorityFeeBips, Transaction, TxDetails, UnsignedTransaction},
    Amount, DispatchCall, Spec,
};
use sov_node_client::NodeClient;
use sov_rollup_interface::{
    crypto::PrivateKey,
    zk::{CryptoSpec, ZkVerifier},
};
use std::{path::Path, sync::Arc};
use tracing::{debug, error, info};

// Import the actual demo-stf Runtime types
use demo_stf::runtime::Runtime as DemoRuntime;
use sov_address::MultiAddressEvm;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_stf_blueprint::Runtime as RuntimeTrait;

/// The rollup's Spec type (must match rollup-ligero configuration)
pub type RollupSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;

/// Configuration for the proof verifier service
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// URL of the rollup node RPC endpoint
    pub node_rpc_url: String,
    /// Private key path for signing non-ZK transactions
    pub signing_key_path: String,
    /// Ligero method ID (code commitment) for proof verification
    pub method_id: [u8; 32],
    /// Maximum number of concurrent verification tasks
    pub max_concurrent_verifications: usize,
    /// Chain ID for transaction authentication
    pub chain_id: u64,
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
}

impl AppState {
    pub fn new(config: ServiceConfig) -> Result<Self, anyhow::Error> {
        let max_permits = config.max_concurrent_verifications;
        let node_client = NodeClient::new_unchecked(&config.node_rpc_url);

        // Load signing key once at startup
        let signing_key = load_private_key(&config.signing_key_path)
            .context("Failed to load signing key at startup")?;

        info!("✓ Loaded signing key from: {}", config.signing_key_path);

        Ok(Self {
            config: Arc::new(config),
            node_client,
            verification_semaphore: Arc::new(tokio::sync::Semaphore::new(max_permits)),
            nonce_counter: Arc::new(tokio::sync::Mutex::new(None)),
            signing_key: Arc::new(signing_key),
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

    #[error("Proof verification failed: {0}")]
    ProofError(String),

    #[error("Failed to submit to node: {0}")]
    SubmissionError(String),

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
            ServiceError::ProofError(msg) => {
                error!("Proof verification error: {}", msg);
                (StatusCode::UNPROCESSABLE_ENTITY, msg.clone())
            }
            ServiceError::SubmissionError(msg) => {
                error!("Submission error: {}", msg);
                (StatusCode::BAD_GATEWAY, msg.clone())
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
        .route("/verify-and-submit", post(verify_and_submit_handler))
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
    let method_id = LigeroCodeCommitment(state.config.method_id);

    // Spawn blocking task for CPU-intensive proof verification
    let proof = proof.to_vec();
    let result = tokio::task::spawn_blocking(move || {
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

    // Wrap in Runtime::Call enum (use the generated runtime call type)
    // The DemoRuntime derives TransactionCallable which provides the Call associated type
    type RuntimeCall = <DemoRuntime<RollupSpec> as DispatchCall>::Decodable;
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

/// Load a private key from file (same format as sov-cli)
fn load_private_key<P: AsRef<Path>>(
    path: P,
) -> Result<<<RollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey> {
    #[derive(Deserialize)]
    struct PrivateKeyAndAddress<S: Spec> {
        private_key: <S::CryptoSpec as CryptoSpec>::PrivateKey,
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
}
