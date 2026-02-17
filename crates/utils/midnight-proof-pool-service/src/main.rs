use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router, ServiceExt};
use mcp_external::commitment_tree::{global_tree_syncer, start_background_tree_sync};
use mcp_external::fvk_service::{fetch_viewer_fvk_bundle, parse_hex_32, ViewerFvkBundle};
use mcp_external::nightstream::Nightstream;
use mcp_external::operations::{deposit, send_funds, transfer, TransferInputNote, DEFAULT_MAX_FEE};
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::provider::Provider;
use mcp_external::server::{McpSpec, McpWalletContext};
use midnight_privacy::{note_commitment, Hash32};
use rand::RngCore;
use reqwest::Client as HttpClient;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sov_bank::TokenId;
use sov_modules_api::Amount;
use sov_proof_verifier_service::{
    create_router as create_verifier_router, AppState, ServiceConfig,
};
use sov_rollup_interface::crypto::{PrivateKey as _, PublicKey as _};
use tempfile::NamedTempFile;
use tokio::net::TcpListener;
use tokio::sync::{Notify, RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tracing_subscriber::EnvFilter;

const DOMAIN: [u8; 32] = [1u8; 32];
const DEFAULT_TREE_RESOLVE_RETRY_ATTEMPTS: u32 = 1;
const DEFAULT_TREE_RESOLVE_RETRY_DELAY_MS: u64 = 750;
const POOL_STATE_TABLE: &str = "pool_wallets";

#[derive(Clone, Debug)]
struct Config {
    auth_token: String,
    max_proofs: usize,
    bind_addr: SocketAddr,
    rollup_rpc_url: String,
    indexer_url: String,
    admin_wallet_private_key_hex: String,
    deposit_amount: u128,
    auto_fund_gas_reserve: u128,
    topup_gas_reserve: u128,
    wallet_setup_backoff_ms: u64,
    sequencer_ready_check_timeout_ms: u64,
    proof_generation_interval_ms: u64,
    max_concurrent_proofs: usize,
    da_connection_string: String,
    nightstream_program_path: String,
    nightstream_proof_service_url: String,
    pool_state_file: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let auth_token = env_required("AUTH_TOKEN")?;
        let max_proofs = env_required("MAX_PROOFS")?
            .parse::<usize>()
            .context("MAX_PROOFS")?;
        anyhow::ensure!(max_proofs > 0, "MAX_PROOFS must be > 0");

        let bind_addr = env_string("PROOF_POOL_BIND_ADDR", "127.0.0.1:11235")
            .parse::<SocketAddr>()
            .context("PROOF_POOL_BIND_ADDR")?;

        let rollup_rpc_url = env_string("ROLLUP_RPC_URL", "http://127.0.0.1:12346");
        let indexer_url = env_string("INDEXER_URL", "http://localhost:13100");

        let admin_wallet_private_key_hex = env_required("ADMIN_WALLET_PRIVATE_KEY")?;
        let da_connection_string = env_required("DA_CONNECTION_STRING")?;

        let deposit_amount = env_u128("DEPOSIT_AMOUNT", 200);

        let gas_reserve_default = 10_000_000u128;
        let auto_fund_gas_reserve =
            env_u128("AUTO_FUND_GAS_RESERVE", gas_reserve_default).max(DEFAULT_MAX_FEE);
        let topup_gas_reserve =
            env_u128("TOPUP_GAS_RESERVE", auto_fund_gas_reserve).max(DEFAULT_MAX_FEE);

        let wallet_setup_backoff_ms = env_u64("WALLET_SETUP_BACKOFF_MS", 1_000).max(50);
        let sequencer_ready_check_timeout_ms =
            env_u64("SEQUENCER_READY_CHECK_TIMEOUT_MS", 2_000).max(100);
        let proof_generation_interval_ms = env_u64("PROOF_GENERATION_INTERVAL_MS", 0);
        let max_concurrent_proofs = env_usize("MAX_CONCURRENT_PROOFS", 5).max(1);

        let nightstream_program_path =
            env_string("NIGHTSTREAM_PROGRAM_PATH", "note_spend_guest");
        let nightstream_proof_service_url =
            env_string("NIGHTSTREAM_PROOF_SERVICE_URL", "http://127.0.0.1:8080");
        let pool_state_file = env_optional_string("POOL_STATE_FILE")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        Ok(Self {
            auth_token,
            max_proofs,
            bind_addr,
            rollup_rpc_url,
            indexer_url,
            admin_wallet_private_key_hex,
            deposit_amount,
            auto_fund_gas_reserve,
            topup_gas_reserve,
            wallet_setup_backoff_ms,
            sequencer_ready_check_timeout_ms,
            proof_generation_interval_ms,
            max_concurrent_proofs,
            da_connection_string,
            nightstream_program_path,
            nightstream_proof_service_url,
            pool_state_file,
        })
    }
}

#[derive(Clone, Debug)]
struct NoteState {
    value: u128,
    rho: Hash32,
    sender_id: Hash32,
}

#[derive(Clone, Debug)]
struct PendingTransfer {
    tx_hash: String,
    next_note: NoteState,
}

// ── Persistence ──────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
struct PersistedNote {
    value: u128,
    rho_hex: String,
    sender_id_hex: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedPending {
    tx_hash: String,
    next_note: PersistedNote,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedWallet {
    wallet_private_key_hex: String,
    privacy_spend_key_hex: String,
    current_note: Option<PersistedNote>,
    pending: Option<PersistedPending>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedPoolState {
    wallets: Vec<PersistedWallet>,
}

impl NoteState {
    fn to_persisted(&self) -> PersistedNote {
        PersistedNote {
            value: self.value,
            rho_hex: hex::encode(self.rho),
            sender_id_hex: hex::encode(self.sender_id),
        }
    }

    fn from_persisted(p: &PersistedNote) -> Result<Self> {
        Ok(Self {
            value: p.value,
            rho: parse_hash32(&p.rho_hex).context("rho")?,
            sender_id: parse_hash32(&p.sender_id_hex).context("sender_id")?,
        })
    }
}

fn parse_hash32(hex_str: &str) -> Result<Hash32> {
    let bytes = hex::decode(hex_str).context("invalid hex")?;
    anyhow::ensure!(bytes.len() == 32, "expected 32 bytes, got {}", bytes.len());
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

#[derive(Clone)]
struct PoolWallet {
    wallet: McpWalletContext,
    wallet_private_key_hex: String,
    privacy_key: PrivacyKey,
    privacy_spend_key_hex: String,
    viewer_fvk_bundle: Option<ViewerFvkBundle>,
    current_note: Option<NoteState>,
    pending: Option<PendingTransfer>,
    generating: bool,
}

/// Upper bound for the semaphore so that `available_permits()` can be used to
/// derive the number of in-flight proof generations at any moment.
const PROOF_SEMAPHORE_CAPACITY: usize = 4096;

struct ServiceState {
    cfg: Config,
    target_max_proofs: AtomicUsize,
    ready_proofs_count: AtomicUsize,
    state_dirty: AtomicBool,
    state_save_notify: Arc<Notify>,
    proof_generation_enabled: AtomicBool,
    proof_generation_interval_ms: AtomicU64,
    max_concurrent_proofs: AtomicUsize,
    provider: Arc<Provider>,
    deposit_provider: Arc<Provider>,
    http: HttpClient,
    verifier_url: String,
    nightstream: Arc<Nightstream>,
    admin_wallet: Arc<McpWalletContext>,
    gas_token_id: TokenId,
    wallets: RwLock<Vec<PoolWallet>>,
    proof_semaphore: Arc<Semaphore>,
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    #[serde(alias = "token")]
    auth_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendQuery {
    #[serde(alias = "token")]
    auth_token: Option<String>,
    proof_quantity: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SendBody {
    proof_quantity: usize,
}

#[derive(Debug, Deserialize)]
struct BurstQuery {
    #[serde(alias = "token")]
    auth_token: Option<String>,
    /// Comma-separated list, e.g. "2,5,10"
    proof_quantities: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BurstBody {
    proof_quantities: Vec<usize>,
}

#[derive(Debug, Deserialize)]
struct MaxProofsQuery {
    #[serde(alias = "token")]
    auth_token: Option<String>,
    max_proofs: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct MaxProofsBody {
    max_proofs: usize,
}

#[derive(Debug, Deserialize)]
struct ProofGenerationQuery {
    #[serde(alias = "token")]
    auth_token: Option<String>,
    proof_generation_state: Option<String>,
    proof_generation_active: Option<bool>,
    proof_generation_interval_ms: Option<u64>,
    // Backward-compatible aliases.
    state: Option<String>,
    interval_ms: Option<u64>,
    max_concurrent_proofs: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ProofGenerationBody {
    proof_generation_state: Option<String>,
    proof_generation_active: Option<bool>,
    proof_generation_interval_ms: Option<u64>,
    // Backward-compatible aliases.
    state: Option<String>,
    interval_ms: Option<u64>,
    max_concurrent_proofs: Option<usize>,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    max_proofs: usize,
    ready_proofs: usize,
    proof_generation_active: bool,
    proof_generation_state: &'static str,
    proof_generation_interval_ms: u64,
    max_concurrent_proofs: usize,
}

#[derive(Debug, Serialize)]
struct SendResponse {
    requested: usize,
    flushed: usize,
    accepted: usize,
    rejected: usize,
    ready_proofs: usize,
}

#[derive(Debug, Serialize)]
struct BurstStep {
    requested: usize,
    flushed: usize,
    accepted: usize,
    rejected: usize,
    ready_proofs: usize,
}

#[derive(Debug, Serialize)]
struct BurstResponse {
    interval_seconds: u64,
    steps: Vec<BurstStep>,
    elapsed_ms: u128,
}

#[derive(Debug, Deserialize)]
struct FlushResultEntry {
    tx_hash: Option<String>,
    accepted: bool,
}

#[derive(Debug, Deserialize)]
struct FlushSummary {
    flushed: usize,
    accepted: usize,
    rejected: usize,
    results: Vec<FlushResultEntry>,
}

#[derive(Debug, Deserialize)]
struct PendingHashesResponse {
    tx_hashes: Vec<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = Config::from_env()?;

    // Avoid a 10s sequencer confirmation wait in mcp-external::operations::transfer.
    std::env::set_var("MCP_TRANSFER_WAIT_MODE", "none");

    let verifier_url = start_embedded_verifier(&cfg, true).await?;
    tracing::info!(verifier_url, "Embedded verifier started (defer mode)");
    let deposit_verifier_url = start_embedded_verifier(&cfg, false).await?;
    tracing::info!(
        deposit_verifier_url,
        "Deposit verifier started (immediate mode)"
    );

    wait_for_sequencer_ready(&cfg.rollup_rpc_url, Duration::from_secs(60)).await?;

    let provider = Arc::new(
        Provider::new(&cfg.rollup_rpc_url, &verifier_url, &cfg.indexer_url)
            .await
            .context("Failed to create rollup provider")?,
    );
    let deposit_provider = Arc::new(
        Provider::new(&cfg.rollup_rpc_url, &deposit_verifier_url, &cfg.indexer_url)
            .await
            .context("Failed to create deposit provider")?,
    );
    start_background_tree_sync(provider.clone());

    let gas_token_id = provider.get_gas_token_id().await.context("gas token id")?;

    let nightstream = Arc::new(Nightstream::new(
        cfg.nightstream_proof_service_url.clone(),
        cfg.nightstream_program_path.clone(),
    ));

    let admin_wallet = Arc::new(
        McpWalletContext::from_private_key_hex(&cfg.admin_wallet_private_key_hex)
            .context("ADMIN_WALLET_PRIVATE_KEY")?,
    );
    let pool_state_sqlite_file = cfg.pool_state_file.as_deref().unwrap_or("(disabled)");
    tracing::info!(
        admin_address = %admin_wallet.get_address(),
        max_proofs = cfg.max_proofs,
        deposit_amount = cfg.deposit_amount,
        gas_reserve = cfg.auto_fund_gas_reserve,
        wallet_setup_parallelism = cfg.max_concurrent_proofs,
        wallet_setup_backoff_ms = cfg.wallet_setup_backoff_ms,
        proof_generation_interval_ms = cfg.proof_generation_interval_ms,
        max_concurrent_proofs = cfg.max_concurrent_proofs,
        pool_state_sqlite_file,
        "Starting proof pool service"
    );

    let state = Arc::new(ServiceState {
        cfg: cfg.clone(),
        target_max_proofs: AtomicUsize::new(cfg.max_proofs),
        ready_proofs_count: AtomicUsize::new(0),
        state_dirty: AtomicBool::new(false),
        state_save_notify: Arc::new(Notify::new()),
        proof_generation_enabled: AtomicBool::new(true),
        proof_generation_interval_ms: AtomicU64::new(cfg.proof_generation_interval_ms),
        max_concurrent_proofs: AtomicUsize::new(cfg.max_concurrent_proofs),
        provider: provider.clone(),
        deposit_provider: deposit_provider.clone(),
        http: HttpClient::new(),
        verifier_url: verifier_url.clone(),
        nightstream: nightstream.clone(),
        admin_wallet: admin_wallet.clone(),
        gas_token_id,
        wallets: RwLock::new(Vec::new()),
        proof_semaphore: Arc::new(Semaphore::new(PROOF_SEMAPHORE_CAPACITY)),
    });
    spawn_state_saver(state.clone());

    // Start the HTTP server immediately; perform wallet setup + initial pool fill in the background.
    // This makes `/health` and `/status` available while the initial MAX_PROOFS are being generated.
    let setup_state = state.clone();
    tokio::spawn(async move {
        // Try to restore from a previous state DB first.
        let restored = match restore_wallets_from_state(&setup_state).await {
            Ok(true) => {
                // Re-fetch viewer FVK bundles (not persisted; cheap to re-fetch).
                if let Err(e) = maybe_fetch_viewer_fvk_bundles(&setup_state).await {
                    tracing::warn!(error = %e, "Failed to re-fetch viewer FVK bundles after restore");
                }
                true
            }
            Ok(false) => false,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "Failed to restore pool state; refusing to start fresh to prevent data loss"
                );
                return;
            }
        };

        if !restored {
            if let Err(e) = setup_wallets_and_fill_pool(setup_state.clone()).await {
                tracing::error!(error = %e, "Startup setup failed; proof pool will not generate proofs");
                return;
            }
        }

        spawn_refill_loop(setup_state.clone());
        spawn_wallet_scale_loop(setup_state);
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/status", get(status_handler))
        .route(
            "/max_proofs",
            post(max_proofs_handler).get(max_proofs_handler_get),
        )
        .route(
            "/proof_generation",
            post(proof_generation_handler).get(proof_generation_handler_get),
        )
        .route("/send", post(send_handler).get(send_handler_get))
        .route("/burst", post(burst_handler).get(burst_handler_get))
        .with_state(state.clone());

    tracing::info!(bind = %cfg.bind_addr, "HTTP server listening");
    let listener = TcpListener::bind(cfg.bind_addr)
        .await
        .context("Failed to bind proof pool service")?;

    let shutdown_state = state.clone();
    let shutdown_signal = async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Shutdown signal received, saving pool state…");
        if let Err(e) = save_pool_state(&shutdown_state).await {
            tracing::error!(error = %e, "Failed to save pool state on shutdown");
        } else {
            tracing::info!("Pool state saved successfully");
        }
    };

    axum::serve(
        listener,
        ServiceExt::<axum::extract::Request>::into_make_service(app),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await
    .context("Failed to serve proof pool service")?;

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

fn proof_generation_state_label(active: bool) -> &'static str {
    if active {
        "started"
    } else {
        "stopped"
    }
}

fn parse_proof_generation_state(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "started" | "start" | "active" | "on" | "true" | "1" => Some(true),
        "stopped" | "stop" | "inactive" | "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_optional_proof_generation_state(raw: Option<&str>) -> Result<Option<bool>, StatusCode> {
    raw.map(|value| parse_proof_generation_state(value).ok_or(StatusCode::BAD_REQUEST))
        .transpose()
}

fn resolve_requested_proof_generation_state(
    canonical_state: Option<&str>,
    legacy_state: Option<&str>,
    active: Option<bool>,
) -> Result<Option<bool>, StatusCode> {
    let canonical_parsed = parse_optional_proof_generation_state(canonical_state)?;
    let legacy_parsed = parse_optional_proof_generation_state(legacy_state)?;
    let state_from_string = match (canonical_parsed, legacy_parsed) {
        (Some(canonical), Some(legacy)) if canonical != legacy => {
            return Err(StatusCode::BAD_REQUEST)
        }
        (Some(canonical), _) => Some(canonical),
        (None, Some(legacy)) => Some(legacy),
        (None, None) => None,
    };

    match (state_from_string, active) {
        (Some(from_string), Some(active)) if from_string != active => Err(StatusCode::BAD_REQUEST),
        (Some(from_string), _) => Ok(Some(from_string)),
        (None, Some(active)) => Ok(Some(active)),
        (None, None) => Ok(None),
    }
}

fn resolve_requested_proof_generation_interval_ms(
    canonical_interval_ms: Option<u64>,
    legacy_interval_ms: Option<u64>,
) -> Result<Option<u64>, StatusCode> {
    match (canonical_interval_ms, legacy_interval_ms) {
        (Some(canonical), Some(legacy)) if canonical != legacy => Err(StatusCode::BAD_REQUEST),
        (Some(canonical), _) => Ok(Some(canonical)),
        (None, Some(legacy)) => Ok(Some(legacy)),
        (None, None) => Ok(None),
    }
}

async fn status_snapshot(state: &Arc<ServiceState>) -> StatusResponse {
    let ready = ready_proofs(state);
    let max_proofs = state.target_max_proofs.load(Ordering::Relaxed);
    let active = state.proof_generation_enabled.load(Ordering::Relaxed);
    let interval_ms = state.proof_generation_interval_ms.load(Ordering::Relaxed);
    let max_concurrent = state.max_concurrent_proofs.load(Ordering::Relaxed);
    StatusResponse {
        max_proofs,
        ready_proofs: ready,
        proof_generation_active: active,
        proof_generation_state: proof_generation_state_label(active),
        proof_generation_interval_ms: interval_ms,
        max_concurrent_proofs: max_concurrent,
    }
}

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn status_handler(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<StatusResponse>, StatusCode> {
    check_auth(&state.cfg.auth_token, query.auth_token.as_deref())?;
    Ok(Json(status_snapshot(&state).await))
}

async fn max_proofs_handler_get(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<MaxProofsQuery>,
) -> Result<Json<StatusResponse>, StatusCode> {
    max_proofs_impl(state, query, None).await
}

async fn max_proofs_handler(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<MaxProofsQuery>,
    body: Option<Json<MaxProofsBody>>,
) -> Result<Json<StatusResponse>, StatusCode> {
    max_proofs_impl(state, query, body.map(|b| b.0)).await
}

async fn proof_generation_handler_get(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<ProofGenerationQuery>,
) -> Result<Json<StatusResponse>, StatusCode> {
    proof_generation_impl(state, query, None).await
}

async fn proof_generation_handler(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<ProofGenerationQuery>,
    body: Option<Json<ProofGenerationBody>>,
) -> Result<Json<StatusResponse>, StatusCode> {
    proof_generation_impl(state, query, body.map(|b| b.0)).await
}

async fn send_handler_get(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<SendQuery>,
) -> Result<Json<SendResponse>, StatusCode> {
    send_impl(state, query, None).await
}

async fn send_handler(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<SendQuery>,
    body: Option<Json<SendBody>>,
) -> Result<Json<SendResponse>, StatusCode> {
    send_impl(state, query, body.map(|b| b.0)).await
}

async fn burst_handler_get(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<BurstQuery>,
) -> Result<Json<BurstResponse>, StatusCode> {
    burst_impl(state, query, None).await
}

async fn burst_handler(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<BurstQuery>,
    body: Option<Json<BurstBody>>,
) -> Result<Json<BurstResponse>, StatusCode> {
    burst_impl(state, query, body.map(|b| b.0)).await
}

async fn max_proofs_impl(
    state: Arc<ServiceState>,
    query: MaxProofsQuery,
    body: Option<MaxProofsBody>,
) -> Result<Json<StatusResponse>, StatusCode> {
    check_auth(&state.cfg.auth_token, query.auth_token.as_deref())?;

    let requested = body.map(|b| b.max_proofs).or(query.max_proofs);
    if let Some(max_proofs) = requested {
        if max_proofs == 0 {
            return Err(StatusCode::BAD_REQUEST);
        }
        state.target_max_proofs.store(max_proofs, Ordering::Relaxed);
        tracing::info!(
            max_proofs,
            wallet_setup_parallelism = state.max_concurrent_proofs.load(Ordering::Relaxed),
            batch_delay_ms = state.cfg.wallet_setup_backoff_ms,
            "Updated MAX_PROOFS target (wallet scale-up is paced)"
        );
    }

    Ok(Json(status_snapshot(&state).await))
}

async fn proof_generation_impl(
    state: Arc<ServiceState>,
    query: ProofGenerationQuery,
    body: Option<ProofGenerationBody>,
) -> Result<Json<StatusResponse>, StatusCode> {
    check_auth(&state.cfg.auth_token, query.auth_token.as_deref())?;

    let body_requested_state = body
        .as_ref()
        .map(|b| {
            resolve_requested_proof_generation_state(
                b.proof_generation_state.as_deref(),
                b.state.as_deref(),
                b.proof_generation_active,
            )
        })
        .transpose()?
        .flatten();
    let query_requested_state = resolve_requested_proof_generation_state(
        query.proof_generation_state.as_deref(),
        query.state.as_deref(),
        query.proof_generation_active,
    )?;
    let requested_state = match (body_requested_state, query_requested_state) {
        (Some(body_state), Some(query_state)) if body_state != query_state => {
            return Err(StatusCode::BAD_REQUEST);
        }
        (Some(body_state), _) => Some(body_state),
        (None, Some(query_state)) => Some(query_state),
        (None, None) => None,
    };

    if let Some(enabled) = requested_state {
        state
            .proof_generation_enabled
            .store(enabled, Ordering::Relaxed);
        tracing::info!(
            proof_generation_active = enabled,
            proof_generation_state = proof_generation_state_label(enabled),
            "Updated proof generation state"
        );
    }

    let body_requested_interval_ms = body
        .as_ref()
        .map(|b| {
            resolve_requested_proof_generation_interval_ms(
                b.proof_generation_interval_ms,
                b.interval_ms,
            )
        })
        .transpose()?
        .flatten();
    let query_requested_interval_ms = resolve_requested_proof_generation_interval_ms(
        query.proof_generation_interval_ms,
        query.interval_ms,
    )?;
    let requested_interval_ms = match (body_requested_interval_ms, query_requested_interval_ms) {
        (Some(body_interval), Some(query_interval)) if body_interval != query_interval => {
            return Err(StatusCode::BAD_REQUEST);
        }
        (Some(body_interval), _) => Some(body_interval),
        (None, Some(query_interval)) => Some(query_interval),
        (None, None) => None,
    };
    if let Some(interval_ms) = requested_interval_ms {
        state
            .proof_generation_interval_ms
            .store(interval_ms, Ordering::Relaxed);
        tracing::info!(
            proof_generation_interval_ms = interval_ms,
            "Updated proof generation throttle interval"
        );
    }

    let requested_max_concurrent = body
        .as_ref()
        .and_then(|b| b.max_concurrent_proofs)
        .or(query.max_concurrent_proofs);
    if let Some(max_concurrent) = requested_max_concurrent {
        if max_concurrent == 0 || max_concurrent > PROOF_SEMAPHORE_CAPACITY {
            return Err(StatusCode::BAD_REQUEST);
        }
        state
            .max_concurrent_proofs
            .store(max_concurrent, Ordering::Relaxed);
        tracing::info!(
            max_concurrent_proofs = max_concurrent,
            "Updated max concurrent proofs"
        );
    }

    Ok(Json(status_snapshot(&state).await))
}

async fn send_impl(
    state: Arc<ServiceState>,
    query: SendQuery,
    body: Option<SendBody>,
) -> Result<Json<SendResponse>, StatusCode> {
    check_auth(&state.cfg.auth_token, query.auth_token.as_deref())?;
    let requested = body
        .map(|b| b.proof_quantity)
        .or(query.proof_quantity)
        .ok_or(StatusCode::BAD_REQUEST)?;
    if requested == 0 {
        return Err(StatusCode::BAD_REQUEST);
    }

    let tx_hashes = collect_pending_hashes(&state, requested).await;
    if tx_hashes.is_empty() {
        let ready = ready_proofs(&state);
        return Ok(Json(SendResponse {
            requested,
            flushed: 0,
            accepted: 0,
            rejected: 0,
            ready_proofs: ready,
        }));
    }

    let flush = flush_verifier(&state, &tx_hashes)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let ready = apply_flush_results(&state, &tx_hashes, &flush).await;

    Ok(Json(SendResponse {
        requested,
        flushed: flush.flushed,
        accepted: flush.accepted,
        rejected: flush.rejected,
        ready_proofs: ready,
    }))
}

async fn burst_impl(
    state: Arc<ServiceState>,
    query: BurstQuery,
    body: Option<BurstBody>,
) -> Result<Json<BurstResponse>, StatusCode> {
    check_auth(&state.cfg.auth_token, query.auth_token.as_deref())?;

    let quantities = if let Some(body) = body {
        body.proof_quantities
    } else if let Some(ref csv) = query.proof_quantities {
        parse_csv_usizes(csv)?
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    if quantities.is_empty() || quantities.iter().any(|&q| q == 0) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let started = Instant::now();
    let interval = Duration::from_secs(2);

    let mut steps: Vec<BurstStep> = Vec::with_capacity(quantities.len());
    for (idx, requested) in quantities.iter().copied().enumerate() {
        if idx > 0 {
            sleep(interval).await;
        }

        let tx_hashes = collect_pending_hashes(&state, requested).await;
        if tx_hashes.is_empty() {
            let ready = ready_proofs(&state);
            steps.push(BurstStep {
                requested,
                flushed: 0,
                accepted: 0,
                rejected: 0,
                ready_proofs: ready,
            });
        } else {
            let flush = flush_verifier(&state, &tx_hashes)
                .await
                .map_err(|_| StatusCode::BAD_GATEWAY)?;

            let ready = apply_flush_results(&state, &tx_hashes, &flush).await;

            steps.push(BurstStep {
                requested,
                flushed: flush.flushed,
                accepted: flush.accepted,
                rejected: flush.rejected,
                ready_proofs: ready,
            });
        }
    }

    Ok(Json(BurstResponse {
        interval_seconds: interval.as_secs(),
        steps,
        elapsed_ms: started.elapsed().as_millis(),
    }))
}

fn check_auth(expected: &str, provided: Option<&str>) -> Result<(), StatusCode> {
    match provided {
        Some(tok) if tok == expected => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

fn parse_csv_usizes(csv: &str) -> Result<Vec<usize>, StatusCode> {
    let mut out: Vec<usize> = Vec::new();
    for part in csv.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        out.push(p.parse::<usize>().map_err(|_| StatusCode::BAD_REQUEST)?);
    }
    if out.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(out)
}

fn ready_proofs(state: &ServiceState) -> usize {
    state.ready_proofs_count.load(Ordering::Relaxed)
}

async fn collect_pending_hashes(state: &Arc<ServiceState>, limit: usize) -> Vec<String> {
    let wallets = state.wallets.read().await;
    wallets
        .iter()
        .filter_map(|w| w.pending.as_ref().map(|p| p.tx_hash.clone()))
        .take(limit)
        .collect()
}

async fn fetch_pending_hashes_from_verifier(state: &Arc<ServiceState>) -> Result<HashSet<String>> {
    let url = format!(
        "{}/midnight-privacy/pending_hashes",
        state.verifier_url.trim_end_matches('/')
    );
    let resp = state
        .http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "verifier pending_hashes failed (status={}): {}",
            status,
            body
        );
    }

    let parsed: PendingHashesResponse =
        serde_json::from_str(&body).context("Failed to parse verifier pending_hashes response")?;
    Ok(parsed.tx_hashes.into_iter().collect())
}

async fn apply_flush_results(
    state: &Arc<ServiceState>,
    requested_tx_hashes: &[String],
    flush: &FlushSummary,
) -> usize {
    let mut accepted_hashes: HashSet<String> = HashSet::new();
    for entry in flush.results.iter() {
        let Some(tx_hash) = entry.tx_hash.as_ref() else {
            continue;
        };
        if entry.accepted {
            accepted_hashes.insert(tx_hash.clone());
        }
    }

    let mut wallets = state.wallets.write().await;
    let mut by_hash: HashMap<String, usize> = HashMap::new();
    for (idx, w) in wallets.iter().enumerate() {
        if let Some(ref p) = w.pending {
            by_hash.insert(p.tx_hash.clone(), idx);
        }
    }

    let mut consumed = 0usize;
    let mut advanced = 0usize;
    for tx_hash in requested_tx_hashes.iter() {
        let Some(&wallet_idx) = by_hash.get(tx_hash) else {
            continue;
        };

        let w = &mut wallets[wallet_idx];
        let Some(pending) = w.pending.clone() else {
            continue;
        };
        if pending.tx_hash != *tx_hash {
            continue;
        }

        if accepted_hashes.contains(tx_hash) {
            w.current_note = Some(pending.next_note);
            advanced += 1;
        }
        w.pending = None;
        consumed += 1;
    }

    let ready_after = wallets.iter().filter(|w| w.pending.is_some()).count();
    drop(wallets);

    state
        .ready_proofs_count
        .store(ready_after, Ordering::Relaxed);
    if consumed > 0 {
        request_pool_state_save(state);
    }

    tracing::info!(
        requested = requested_tx_hashes.len(),
        flushed = flush.flushed,
        consumed,
        advanced,
        ready_after,
        "Applied flush results and consumed requested pending proofs"
    );

    ready_after
}

fn spawn_refill_loop(state: Arc<ServiceState>) {
    tokio::spawn(async move {
        let mut next_allowed_generation = tokio::time::Instant::now();
        loop {
            if !state.proof_generation_enabled.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            let interval_ms = state.proof_generation_interval_ms.load(Ordering::Relaxed);
            if interval_ms > 0 {
                let now = tokio::time::Instant::now();
                if now < next_allowed_generation {
                    sleep(next_allowed_generation - now).await;
                    continue;
                }
            }

            let ready = ready_proofs(&state);
            let target = state.target_max_proofs.load(Ordering::Relaxed);
            if ready >= target {
                sleep(Duration::from_millis(200)).await;
                continue;
            }
            let deficit = target - ready;

            let max_concurrent = state.max_concurrent_proofs.load(Ordering::Relaxed);
            let in_flight = PROOF_SEMAPHORE_CAPACITY - state.proof_semaphore.available_permits();
            let room = max_concurrent.saturating_sub(in_flight);
            let launches_target = deficit.min(room);

            if launches_target == 0 {
                sleep(Duration::from_millis(50)).await;
                continue;
            }

            let mut launched = 0usize;
            for _ in 0..launches_target {
                let permit = match state.proof_semaphore.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => break,
                };

                let wallet_idx = match pick_wallet_to_generate(&state).await {
                    Some(idx) => idx,
                    None => {
                        drop(permit);
                        break;
                    }
                };

                let st = state.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = generate_pending_for_wallet(&st, wallet_idx).await {
                        tracing::warn!(wallet_idx, error = %e, "Failed to generate pending proof");
                    }
                });
                launched += 1;
            }

            if launched == 0 {
                sleep(Duration::from_millis(50)).await;
                continue;
            }

            if interval_ms > 0 {
                next_allowed_generation =
                    tokio::time::Instant::now() + Duration::from_millis(interval_ms);
            } else {
                tokio::task::yield_now().await;
            }
        }
    });
}

fn wallet_setup_parallelism(state: &Arc<ServiceState>) -> usize {
    state.max_concurrent_proofs.load(Ordering::Relaxed).max(1)
}

fn spawn_wallet_scale_loop(state: Arc<ServiceState>) {
    tokio::spawn(async move {
        loop {
            let target = state.target_max_proofs.load(Ordering::Relaxed);
            let current = state.wallets.read().await.len();
            if current < target {
                if !sequencer_ready_for_wallet_setup(&state).await {
                    tracing::warn!(
                        current,
                        target,
                        backoff_ms = state.cfg.wallet_setup_backoff_ms,
                        "Sequencer is not ready; delaying wallet scale-up batch"
                    );
                    sleep(Duration::from_millis(state.cfg.wallet_setup_backoff_ms)).await;
                    continue;
                }

                let missing = target - current;
                let to_add = missing.min(wallet_setup_parallelism(&state));
                tracing::info!(
                    current,
                    target,
                    missing,
                    batch_size = to_add,
                    wallet_setup_parallelism = wallet_setup_parallelism(&state),
                    "Scaling wallet pool up in batch"
                );
                if let Err(e) = setup_and_append_wallets(&state, to_add).await {
                    tracing::error!(error = %e, "Failed to scale wallet pool");
                    sleep(Duration::from_millis(state.cfg.wallet_setup_backoff_ms)).await;
                } else {
                    // Smooth large max_proofs increases by pacing successful scale-up batches.
                    sleep(Duration::from_millis(state.cfg.wallet_setup_backoff_ms)).await;
                }
                continue;
            }
            sleep(Duration::from_millis(state.cfg.wallet_setup_backoff_ms)).await;
        }
    });
}

async fn sequencer_ready_for_wallet_setup(state: &Arc<ServiceState>) -> bool {
    let ready_url = format!(
        "{}/sequencer/ready",
        state.cfg.rollup_rpc_url.trim_end_matches('/')
    );

    let req = state.http.get(&ready_url).send();
    match timeout(
        Duration::from_millis(state.cfg.sequencer_ready_check_timeout_ms),
        req,
    )
    .await
    {
        Ok(Ok(resp)) => resp.status().is_success(),
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "Failed to query sequencer readiness");
            false
        }
        Err(_) => {
            tracing::debug!(
                timeout_ms = state.cfg.sequencer_ready_check_timeout_ms,
                "Timed out querying sequencer readiness"
            );
            false
        }
    }
}

async fn pick_wallet_to_generate(state: &Arc<ServiceState>) -> Option<usize> {
    let mut wallets = state.wallets.write().await;
    for (idx, w) in wallets.iter_mut().enumerate() {
        if w.generating {
            continue;
        }
        if w.current_note.is_none() {
            continue;
        }
        if w.pending.is_some() {
            continue;
        }
        w.generating = true;
        return Some(idx);
    }
    None
}

async fn generate_pending_for_wallet(state: &Arc<ServiceState>, wallet_idx: usize) -> Result<()> {
    let result: Result<()> = async {
        let (wallet, privacy_key, viewer_fvk_bundle, current_note) = {
            let wallets = state.wallets.read().await;
            let w = wallets
                .get(wallet_idx)
                .ok_or_else(|| anyhow!("wallet idx out of range"))?;
            let note = w
                .current_note
                .clone()
                .ok_or_else(|| anyhow!("wallet has no current note"))?;
            (
                w.wallet.clone(),
                w.privacy_key.clone(),
                w.viewer_fvk_bundle.clone(),
                note,
            )
        };

        wait_for_note_in_tree(state.provider.as_ref(), &privacy_key, &current_note).await?;
        ensure_wallet_gas_reserve(state, &wallet).await?;

        let spend_sk = *privacy_key
            .spend_sk()
            .ok_or_else(|| anyhow!("privacy key missing spend_sk"))?;
        let pk_ivk_owner = privacy_key.pk_ivk(&DOMAIN);
        let destination_pk_spend = *privacy_key.pk();
        let destination_pk_ivk = privacy_key.pk_ivk(&DOMAIN);

        let input = TransferInputNote {
            value: current_note.value,
            rho: current_note.rho,
            sender_id: current_note.sender_id,
        };

        let tree_retry_attempts = tree_resolve_retry_attempts();
        let tree_retry_delay = Duration::from_millis(tree_resolve_retry_delay_ms());
        let mut transfer_attempt: u32 = 0;
        let res = loop {
            transfer_attempt += 1;
            let res = transfer(
                state.nightstream.as_ref(),
                state.provider.as_ref(),
                &wallet,
                spend_sk,
                pk_ivk_owner,
                current_note.value,
                vec![input.clone()],
                destination_pk_spend,
                destination_pk_ivk,
                viewer_fvk_bundle.clone(),
            )
            .await;

            match res {
                Ok(ok) => break ok,
                Err(e) => {
                    let error_text = format!("{:#}", e);
                    if is_tree_positions_resolution_error(&error_text)
                        && transfer_attempt <= tree_retry_attempts + 1
                    {
                        tracing::warn!(
                            wallet_idx,
                            attempt = transfer_attempt,
                            max_attempts = tree_retry_attempts + 1,
                            retry_delay_ms = tree_retry_delay.as_millis(),
                            error = %error_text,
                            "Self-transfer hit transient commitment-tree lag; retrying"
                        );
                        sleep(tree_retry_delay).await;
                        continue;
                    }
                    if is_invalid_anchor_root_error(&error_text)
                        && transfer_attempt <= tree_retry_attempts + 1
                    {
                        tracing::warn!(
                            wallet_idx,
                            attempt = transfer_attempt,
                            max_attempts = tree_retry_attempts + 1,
                            retry_delay_ms = tree_retry_delay.as_millis(),
                            error = %error_text,
                            "Self-transfer rejected due to stale/invalid anchor root; resetting commitment-tree cache and retrying"
                        );
                        global_tree_syncer().reset_cache().await;
                        sleep(tree_retry_delay).await;
                        continue;
                    }
                    return Err(e).context("transfer (self)");
                }
            }
        };

        let next_note = NoteState {
            value: current_note.value,
            rho: res.output_rho,
            sender_id: privacy_key.recipient(&DOMAIN),
        };

        let became_pending;
        {
            let mut wallets = state.wallets.write().await;
            let w = wallets
                .get_mut(wallet_idx)
                .ok_or_else(|| anyhow!("wallet idx out of range (write)"))?;
            let was_pending = w.pending.is_some();
            w.pending = Some(PendingTransfer {
                tx_hash: res.tx_hash.clone(),
                next_note,
            });
            w.generating = false;
            became_pending = !was_pending;
        }

        if became_pending {
            state.ready_proofs_count.fetch_add(1, Ordering::Relaxed);
            request_pool_state_save(state);
        }

        Ok(())
    }
    .await;

    if result.is_err() {
        let mut wallets = state.wallets.write().await;
        if let Some(w) = wallets.get_mut(wallet_idx) {
            w.generating = false;
        }
    }

    result
}

async fn ensure_wallet_gas_reserve(
    state: &Arc<ServiceState>,
    wallet: &McpWalletContext,
) -> Result<()> {
    let wallet_address = wallet.get_address();
    let balance = state
        .provider
        .get_balance::<McpSpec>(&wallet_address, &state.gas_token_id)
        .await
        .context("get wallet balance")?
        .0;

    if balance >= DEFAULT_MAX_FEE {
        return Ok(());
    }

    tracing::warn!(
        address = %wallet.get_address(),
        balance,
        "Wallet balance below DEFAULT_MAX_FEE; topping up"
    );

    let to_addr = wallet_address.to_string();
    let _ = send_funds(
        state.provider.as_ref(),
        state.admin_wallet.as_ref(),
        &to_addr,
        &state.gas_token_id,
        Amount::from(state.cfg.topup_gas_reserve),
    )
    .await
    .context("send_funds topup")?;

    Ok(())
}

async fn setup_wallets_and_fill_pool(state: Arc<ServiceState>) -> Result<()> {
    let started = Instant::now();

    let l2_funding_amount = state.cfg.deposit_amount + state.cfg.auto_fund_gas_reserve;

    let mut wallets: Vec<PoolWallet> = Vec::with_capacity(state.cfg.max_proofs);
    for _ in 0..state.cfg.max_proofs {
        let wallet_key_hex = generate_key_hex();
        let privacy_key_hex = generate_key_hex();
        let wallet = McpWalletContext::from_private_key_hex(&wallet_key_hex)?;
        let privacy_key = PrivacyKey::from_hex(&privacy_key_hex)?;
        wallets.push(PoolWallet {
            wallet,
            wallet_private_key_hex: wallet_key_hex,
            privacy_key,
            privacy_spend_key_hex: privacy_key_hex,
            viewer_fvk_bundle: None,
            current_note: None,
            pending: None,
            generating: false,
        });
    }
    *state.wallets.write().await = wallets;

    maybe_fetch_viewer_fvk_bundles(&state).await?;

    tracing::info!(
        max_proofs = state.cfg.max_proofs,
        l2_funding_amount,
        "Funding wallets from admin"
    );
    fund_wallets(&state, l2_funding_amount).await?;
    wait_for_wallet_balances(&state, l2_funding_amount).await?;

    tracing::info!(
        deposit_amount = state.cfg.deposit_amount,
        "Submitting deposits"
    );
    let deposit_notes = submit_deposits(&state).await?;

    tracing::info!("Waiting for deposit notes to be indexed");
    let privacy_keys: Vec<PrivacyKey> = {
        let wallets = state.wallets.read().await;
        wallets.iter().map(|w| w.privacy_key.clone()).collect()
    };
    for (wallet_idx, note) in deposit_notes.iter().enumerate() {
        let privacy_key = privacy_keys
            .get(wallet_idx)
            .ok_or_else(|| anyhow!("wallet idx out of range"))?;
        wait_for_note_in_tree(state.provider.as_ref(), privacy_key, note).await?;
    }

    {
        let mut wallets = state.wallets.write().await;
        for (idx, note) in deposit_notes.into_iter().enumerate() {
            if let Some(w) = wallets.get_mut(idx) {
                w.current_note = Some(note);
            }
        }
    }

    let ready = ready_proofs(&state);
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        ready,
        "Startup complete"
    );
    request_pool_state_save(&state);

    Ok(())
}

async fn setup_and_append_wallets(state: &Arc<ServiceState>, count: usize) -> Result<()> {
    if count == 0 {
        return Ok(());
    }

    let started = Instant::now();
    let l2_funding_amount = state.cfg.deposit_amount + state.cfg.auto_fund_gas_reserve;
    tracing::info!(
        count,
        l2_funding_amount,
        deposit_amount = state.cfg.deposit_amount,
        "Setting up new wallets"
    );

    let mut new_wallets: Vec<PoolWallet> = Vec::with_capacity(count);
    for _ in 0..count {
        let wallet_key_hex = generate_key_hex();
        let privacy_key_hex = generate_key_hex();
        let wallet = McpWalletContext::from_private_key_hex(&wallet_key_hex)?;
        let privacy_key = PrivacyKey::from_hex(&privacy_key_hex)?;
        new_wallets.push(PoolWallet {
            wallet,
            wallet_private_key_hex: wallet_key_hex,
            privacy_key,
            privacy_spend_key_hex: privacy_key_hex,
            viewer_fvk_bundle: None,
            current_note: None,
            pending: None,
            generating: false,
        });
    }

    maybe_fetch_viewer_fvk_bundles_for_wallets(state, &mut new_wallets).await?;

    fund_wallets_list(state, &new_wallets, l2_funding_amount).await?;
    wait_for_wallet_balances_list(state, &new_wallets, l2_funding_amount).await?;

    tracing::info!("Submitting new deposits");
    let deposit_notes = submit_deposits_list(state, &new_wallets).await?;

    tracing::info!("Waiting for new deposit notes to be indexed");
    for (wallet_idx, note) in deposit_notes.iter().enumerate() {
        let w = new_wallets
            .get(wallet_idx)
            .ok_or_else(|| anyhow!("wallet idx out of range"))?;
        wait_for_note_in_tree(state.provider.as_ref(), &w.privacy_key, note).await?;
    }

    for (w, note) in new_wallets.iter_mut().zip(deposit_notes.into_iter()) {
        w.current_note = Some(note);
    }

    let (start_idx, total) = {
        let mut wallets = state.wallets.write().await;
        let start_idx = wallets.len();
        wallets.extend(new_wallets);
        (start_idx, wallets.len())
    };

    tracing::info!(
        start_idx,
        total,
        elapsed_ms = started.elapsed().as_millis(),
        "Wallet pool scaled up"
    );
    request_pool_state_save(state);
    Ok(())
}

async fn maybe_fetch_viewer_fvk_bundles_for_wallets(
    state: &Arc<ServiceState>,
    wallets: &mut [PoolWallet],
) -> Result<()> {
    let pool_fvk_pk_raw = std::env::var("POOL_FVK_PK").ok();
    let pool_fvk_pk_raw = pool_fvk_pk_raw.map(|v| v.trim().to_string());
    let Some(pool_fvk_pk_raw) = pool_fvk_pk_raw else {
        return Ok(());
    };
    if pool_fvk_pk_raw.is_empty() {
        return Ok(());
    }

    let pool_fvk_pk = parse_hex_32("POOL_FVK_PK", &pool_fvk_pk_raw)?;
    tracing::info!(
        wallets = wallets.len(),
        "POOL_FVK_PK is set; fetching viewer FVK bundles (1 per wallet) from midnight-fvk-service"
    );

    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<(usize, ViewerFvkBundle)>> = JoinSet::new();

    for (idx, w) in wallets.iter().enumerate() {
        let http = state.http.clone();
        let wallet_address = w.wallet.get_address().to_string();
        let shielded_address = w.privacy_key.privacy_address(&DOMAIN).to_string();
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let bundle = fetch_viewer_fvk_bundle(
                &http,
                Some(pool_fvk_pk),
                Some(&shielded_address),
                Some(&wallet_address),
            )
            .await
            .with_context(|| format!("fetch_viewer_fvk_bundle wallet_idx={idx}"))?;
            Ok((idx, bundle))
        });
    }

    let mut out: Vec<Option<ViewerFvkBundle>> = vec![None; wallets.len()];
    while let Some(res) = join_set.join_next().await {
        let (idx, bundle) = res??;
        out[idx] = Some(bundle);
    }

    for (idx, bundle) in out.into_iter().enumerate() {
        let bundle = bundle.ok_or_else(|| anyhow!("missing viewer bundle for wallet {idx}"))?;
        if let Some(w) = wallets.get_mut(idx) {
            w.viewer_fvk_bundle = Some(bundle);
        }
    }

    Ok(())
}

async fn fund_wallets_list(
    state: &Arc<ServiceState>,
    wallets: &[PoolWallet],
    amount: u128,
) -> Result<()> {
    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();

    for w in wallets.iter() {
        let provider = state.provider.clone();
        let admin = state.admin_wallet.clone();
        let token_id = state.gas_token_id.clone();
        let to_addr = w.wallet.get_address().to_string();
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let _res = send_funds(
                provider.as_ref(),
                admin.as_ref(),
                &to_addr,
                &token_id,
                Amount::from(amount),
            )
            .await?;
            Ok(())
        });
    }

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn wait_for_wallet_balances_list(
    state: &Arc<ServiceState>,
    wallets: &[PoolWallet],
    min_balance: u128,
) -> Result<()> {
    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();

    for w in wallets.iter() {
        let provider = state.provider.clone();
        let token_id = state.gas_token_id.clone();
        let addr = w.wallet.get_address();
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let start = Instant::now();
            let deadline = start + Duration::from_secs(60);
            let mut last_err: Option<anyhow::Error> = None;
            loop {
                let bal = match provider.get_balance::<McpSpec>(&addr, &token_id).await {
                    Ok(amount) => {
                        if last_err.is_some() {
                            last_err = None;
                        }
                        amount.0
                    }
                    Err(e) => {
                        tracing::debug!(address = %addr, error = %e, "Failed to query L2 balance while waiting for funding");
                        last_err = Some(e);
                        0
                    }
                };
                if bal >= min_balance {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    if let Some(e) = last_err {
                        bail!(
                            "Timed out waiting for wallet to be funded (have {}, need {}): {}",
                            bal,
                            min_balance,
                            e
                        );
                    }
                    bail!(
                        "Timed out waiting for wallet to be funded (have {}, need {})",
                        bal,
                        min_balance
                    );
                }
                sleep(Duration::from_secs(2)).await;
            }
        });
    }

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn submit_deposits_list(
    state: &Arc<ServiceState>,
    wallets: &[PoolWallet],
) -> Result<Vec<NoteState>> {
    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<(usize, NoteState)>> = JoinSet::new();

    for (idx, w) in wallets.iter().enumerate() {
        let provider = state.deposit_provider.clone();
        let wallet = w.wallet.clone();
        let privacy_key = w.privacy_key.clone();
        let deposit_amount = state.cfg.deposit_amount;
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let res = deposit(provider.as_ref(), &wallet, deposit_amount, &privacy_key).await?;
            Ok((
                idx,
                NoteState {
                    value: deposit_amount,
                    rho: res.rho,
                    sender_id: privacy_key.recipient(&DOMAIN),
                },
            ))
        });
    }

    let mut out: Vec<Option<NoteState>> = vec![None; wallets.len()];
    while let Some(res) = join_set.join_next().await {
        let (idx, note) = res??;
        out[idx] = Some(note);
    }

    out.into_iter()
        .map(|o| o.ok_or_else(|| anyhow!("missing deposit note result")))
        .collect()
}

async fn maybe_fetch_viewer_fvk_bundles(state: &Arc<ServiceState>) -> Result<()> {
    let pool_fvk_pk_raw = std::env::var("POOL_FVK_PK").ok();
    let pool_fvk_pk_raw = pool_fvk_pk_raw.map(|v| v.trim().to_string());
    let Some(pool_fvk_pk_raw) = pool_fvk_pk_raw else {
        return Ok(());
    };
    if pool_fvk_pk_raw.is_empty() {
        return Ok(());
    }

    let pool_fvk_pk = parse_hex_32("POOL_FVK_PK", &pool_fvk_pk_raw)?;
    tracing::info!(
        wallets = state.cfg.max_proofs,
        "POOL_FVK_PK is set; fetching viewer FVK bundles (1 per wallet) from midnight-fvk-service"
    );

    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<(usize, ViewerFvkBundle)>> = JoinSet::new();
    let wallet_targets: Vec<(usize, String, String)> = {
        let wallets = state.wallets.read().await;
        wallets
            .iter()
            .enumerate()
            .map(|(idx, w)| {
                (
                    idx,
                    w.wallet.get_address().to_string(),
                    w.privacy_key.privacy_address(&DOMAIN).to_string(),
                )
            })
            .collect()
    };

    for (idx, wallet_address, shielded_address) in wallet_targets.iter().cloned() {
        let http = state.http.clone();
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let bundle = fetch_viewer_fvk_bundle(
                &http,
                Some(pool_fvk_pk),
                Some(&shielded_address),
                Some(&wallet_address),
            )
            .await
            .with_context(|| format!("fetch_viewer_fvk_bundle wallet_idx={idx}"))?;
            Ok((idx, bundle))
        });
    }

    let mut out: Vec<Option<ViewerFvkBundle>> = vec![None; wallet_targets.len()];
    while let Some(res) = join_set.join_next().await {
        let (idx, bundle) = res??;
        out[idx] = Some(bundle);
    }

    let mut wallets = state.wallets.write().await;
    for (idx, bundle) in out.into_iter().enumerate() {
        let bundle = bundle.ok_or_else(|| anyhow!("missing viewer bundle for wallet {idx}"))?;
        if let Some(w) = wallets.get_mut(idx) {
            w.viewer_fvk_bundle = Some(bundle);
        }
    }

    Ok(())
}

async fn fund_wallets(state: &Arc<ServiceState>, amount: u128) -> Result<()> {
    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();

    let to_addrs: Vec<String> = {
        let wallets = state.wallets.read().await;
        wallets
            .iter()
            .map(|w| w.wallet.get_address().to_string())
            .collect()
    };
    for to_addr in to_addrs {
        let provider = state.provider.clone();
        let admin = state.admin_wallet.clone();
        let token_id = state.gas_token_id.clone();
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let _res = send_funds(
                provider.as_ref(),
                admin.as_ref(),
                &to_addr,
                &token_id,
                Amount::from(amount),
            )
            .await?;
            Ok(())
        });
    }

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn wait_for_wallet_balances(state: &Arc<ServiceState>, min_balance: u128) -> Result<()> {
    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();

    let addrs: Vec<_> = {
        let wallets = state.wallets.read().await;
        wallets.iter().map(|w| w.wallet.get_address()).collect()
    };
    for addr in addrs {
        let provider = state.provider.clone();
        let token_id = state.gas_token_id.clone();
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let start = Instant::now();
            let deadline = start + Duration::from_secs(60);
            let mut last_err: Option<anyhow::Error> = None;
            loop {
                let bal = match provider.get_balance::<McpSpec>(&addr, &token_id).await {
                    Ok(amount) => {
                        if last_err.is_some() {
                            last_err = None;
                        }
                        amount.0
                    }
                    Err(e) => {
                        // The bank balance endpoint returns 404 until the first transfer to that
                        // address becomes queryable. Treat any transient query error as
                        // "balance=0" and keep polling until the deadline.
                        tracing::debug!(address = %addr, error = %e, "Failed to query L2 balance while waiting for funding");
                        last_err = Some(e);
                        0
                    }
                };
                if bal >= min_balance {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    if let Some(e) = last_err {
                        bail!(
                            "Timed out waiting for wallet to be funded (have {}, need {}): {}",
                            bal,
                            min_balance,
                            e
                        );
                    }
                    bail!(
                        "Timed out waiting for wallet to be funded (have {}, need {})",
                        bal,
                        min_balance
                    );
                }
                sleep(Duration::from_secs(2)).await;
            }
        });
    }

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn submit_deposits(state: &Arc<ServiceState>) -> Result<Vec<NoteState>> {
    let sem = Arc::new(Semaphore::new(wallet_setup_parallelism(state)));
    let mut join_set: JoinSet<Result<(usize, NoteState)>> = JoinSet::new();

    let wallet_inputs: Vec<(usize, McpWalletContext, PrivacyKey)> = {
        let wallets = state.wallets.read().await;
        wallets
            .iter()
            .enumerate()
            .map(|(idx, w)| (idx, w.wallet.clone(), w.privacy_key.clone()))
            .collect()
    };
    let wallets_len = wallet_inputs.len();
    for (idx, wallet, privacy_key) in wallet_inputs {
        let provider = state.deposit_provider.clone();
        let deposit_amount = state.cfg.deposit_amount;
        let permit = sem.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            let res = deposit(provider.as_ref(), &wallet, deposit_amount, &privacy_key).await?;
            Ok((
                idx,
                NoteState {
                    value: deposit_amount,
                    rho: res.rho,
                    sender_id: privacy_key.recipient(&DOMAIN),
                },
            ))
        });
    }

    let mut out: Vec<Option<NoteState>> = vec![None; wallets_len];
    while let Some(res) = join_set.join_next().await {
        let (idx, note) = res??;
        out[idx] = Some(note);
    }

    out.into_iter()
        .map(|o| o.ok_or_else(|| anyhow!("missing deposit note result")))
        .collect()
}

async fn flush_verifier(state: &Arc<ServiceState>, tx_hashes: &[String]) -> Result<FlushSummary> {
    let mut url = format!(
        "{}/midnight-privacy/flush",
        state.verifier_url.trim_end_matches('/')
    );
    let mut query_params: Vec<String> = vec!["wait_for_db=true".to_string()];
    if !tx_hashes.is_empty() {
        query_params.push(format!("limit={}", tx_hashes.len()));
    }
    url = format!("{url}?{}", query_params.join("&"));

    let resp = state
        .http
        .post(&url)
        .json(&serde_json::json!({ "tx_hashes": tx_hashes }))
        .send()
        .await
        .with_context(|| format!("POST {}", url))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("verifier flush failed (status={}): {}", status, body);
    }

    serde_json::from_str(&body).context("Failed to parse verifier flush response")
}
async fn wait_for_note_in_tree(
    provider: &Provider,
    privacy_key: &PrivacyKey,
    note: &NoteState,
) -> Result<()> {
    let value_u64: u64 = note
        .value
        .try_into()
        .context("note value does not fit into u64")?;
    let recipient = privacy_key.recipient(&DOMAIN);
    let cm = note_commitment(&DOMAIN, value_u64, &note.rho, &recipient, &note.sender_id);

    let (_root, _pos, _sib) = global_tree_syncer()
        .resolve_positions_and_openings(provider, &[cm])
        .await
        .context("waiting for note commitment position")?;
    Ok(())
}

async fn wait_for_sequencer_ready(node_url: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let base = node_url.trim_end_matches('/');
    let url = format!("{}/sequencer/ready", base);
    let http = HttpClient::new();

    loop {
        if start.elapsed() > timeout {
            bail!("Timeout waiting for sequencer readiness at {}", url);
        }

        if http
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return Ok(());
        }

        sleep(Duration::from_millis(500)).await;
    }
}

fn compute_nightstream_method_id(_program: &str) -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use sov_nightstream_adapter::circuits::note_spend_rom;
    let method_id: [u8; 32] = Sha256::digest(&note_spend_rom::NOTE_SPEND_ROM)[..]
        .try_into()
        .map_err(|_| anyhow!("SHA-256 digest should be 32 bytes"))?;
    Ok(method_id)
}

async fn start_embedded_verifier(cfg: &Config, defer_sequencer_submission: bool) -> Result<String> {
    let method_id = compute_nightstream_method_id(&cfg.nightstream_program_path)?;

    type RollupSpec = sov_proof_verifier_service::RollupSpec;
    type PrivKey = <<RollupSpec as sov_modules_api::Spec>::CryptoSpec as sov_rollup_interface::zk::CryptoSpec>::PrivateKey;

    let sk: PrivKey = PrivKey::generate();
    let pk = sk.pub_key();
    let addr: <RollupSpec as sov_modules_api::Spec>::Address = pk.credential_id().into();

    #[derive(Serialize)]
    struct KeyFile<'a, PK, Addr> {
        private_key: &'a PK,
        address: Addr,
    }

    let tmpkey = NamedTempFile::new().context("Failed to create temp key file")?;
    std::fs::write(
        tmpkey.path(),
        serde_json::to_string(&KeyFile {
            private_key: &sk,
            address: addr,
        })?,
    )?;

    let verifier_cfg = ServiceConfig {
        node_rpc_url: cfg.rollup_rpc_url.clone(),
        signing_key_path: tmpkey.path().to_string_lossy().to_string(),
        value_setter_method_id: None,
        midnight_method_id: Some(method_id),
        // Keep embedded verifier request-cap well above proof-pool runtime tuning so
        // `max_concurrent_proofs` from proof-pool remains the effective single knob.
        max_concurrent_verifications: PROOF_SEMAPHORE_CAPACITY,
        chain_id: 1,
        da_connection_string: cfg.da_connection_string.clone(),
        defer_sequencer_submission,
    };

    let state = AppState::new(verifier_cfg)
        .await
        .context("Failed to create verifier AppState")?;
    let app = create_verifier_router(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind embedded verifier")?;
    let service_addr = listener.local_addr()?;
    let verifier_url = format!("http://{}", service_addr);

    tokio::spawn(async move {
        axum::serve(
            listener,
            ServiceExt::<axum::extract::Request>::into_make_service(app),
        )
        .await
        .expect("Embedded verifier crashed");
    });
    std::mem::forget(tmpkey);

    let hc = HttpClient::new();
    let _ = hc.get(format!("{}/health", verifier_url)).send().await;

    Ok(verifier_url)
}

// ── Pool state persistence ───────────────────────────────────────────────────

fn configured_pool_state_sqlite_path(cfg: &Config) -> Option<&str> {
    cfg.pool_state_file.as_deref()
}

fn open_pool_state_sqlite(db_path: &str) -> Result<Connection> {
    if let Some(parent) = Path::new(db_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create pool state DB parent directory {}",
                    parent.display()
                )
            })?;
        }
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open pool state SQLite DB {}", db_path))?;
    conn.execute_batch(&format!(
        "\
PRAGMA journal_mode=WAL;\
PRAGMA synchronous=NORMAL;\
CREATE TABLE IF NOT EXISTS {POOL_STATE_TABLE} (\
    wallet_idx INTEGER PRIMARY KEY,\
    wallet_private_key_hex TEXT NOT NULL,\
    privacy_spend_key_hex TEXT NOT NULL,\
    current_note_value TEXT,\
    current_note_rho_hex TEXT,\
    current_note_sender_id_hex TEXT,\
    pending_tx_hash TEXT,\
    pending_next_note_value TEXT,\
    pending_next_note_rho_hex TEXT,\
    pending_next_note_sender_id_hex TEXT\
);"
    ))
    .with_context(|| {
        format!(
            "Failed to initialize pool state SQLite schema in {}",
            db_path
        )
    })?;
    ensure_pool_state_schema(&conn, db_path)?;
    Ok(conn)
}

fn ensure_pool_state_schema(conn: &Connection, db_path: &str) -> Result<()> {
    let pragma_sql = format!("PRAGMA table_info({POOL_STATE_TABLE})");
    let mut stmt = conn
        .prepare(&pragma_sql)
        .with_context(|| format!("Failed to inspect pool state schema in {}", db_path))?;
    let mut rows = stmt
        .query([])
        .with_context(|| format!("Failed to query pool state schema in {}", db_path))?;

    let mut existing_columns: HashSet<String> = HashSet::new();
    while let Some(row) = rows
        .next()
        .with_context(|| format!("Failed to read pool state schema row in {}", db_path))?
    {
        let name: String = row
            .get(1)
            .with_context(|| format!("Failed to parse pool state column name in {}", db_path))?;
        existing_columns.insert(name);
    }

    if !existing_columns.contains("wallet_idx")
        || !existing_columns.contains("wallet_private_key_hex")
        || !existing_columns.contains("privacy_spend_key_hex")
    {
        bail!(
            "Pool state DB {} has incompatible schema for table {} (missing required base columns)",
            db_path,
            POOL_STATE_TABLE
        );
    }

    for (column_name, sql_type) in [
        ("current_note_value", "TEXT"),
        ("current_note_rho_hex", "TEXT"),
        ("current_note_sender_id_hex", "TEXT"),
        ("pending_tx_hash", "TEXT"),
        ("pending_next_note_value", "TEXT"),
        ("pending_next_note_rho_hex", "TEXT"),
        ("pending_next_note_sender_id_hex", "TEXT"),
    ] {
        if existing_columns.contains(column_name) {
            continue;
        }
        let alter_sql =
            format!("ALTER TABLE {POOL_STATE_TABLE} ADD COLUMN {column_name} {sql_type}");
        conn.execute(&alter_sql, []).with_context(|| {
            format!(
                "Failed to add missing column {} to pool state DB {}",
                column_name, db_path
            )
        })?;
        tracing::warn!(
            db_path,
            column_name,
            "Auto-migrated pool state DB by adding missing column"
        );
    }

    Ok(())
}

async fn save_pool_state(state: &Arc<ServiceState>) -> Result<()> {
    let db_path = match configured_pool_state_sqlite_path(&state.cfg) {
        Some(path) => path.to_string(),
        None => return Ok(()), // persistence not enabled
    };

    let persisted = {
        let wallets = state.wallets.read().await;
        PersistedPoolState {
            wallets: wallets
                .iter()
                .map(|w| PersistedWallet {
                    wallet_private_key_hex: w.wallet_private_key_hex.clone(),
                    privacy_spend_key_hex: w.privacy_spend_key_hex.clone(),
                    current_note: w.current_note.as_ref().map(|n| n.to_persisted()),
                    pending: w.pending.as_ref().map(|p| PersistedPending {
                        tx_hash: p.tx_hash.clone(),
                        next_note: p.next_note.to_persisted(),
                    }),
                })
                .collect(),
        }
    };

    tokio::task::spawn_blocking(move || save_pool_state_sqlite_sync(&db_path, persisted))
        .await
        .context("Pool state SQLite save task failed to join")?
}

fn save_pool_state_sqlite_sync(db_path: &str, persisted: PersistedPoolState) -> Result<()> {
    let mut conn = open_pool_state_sqlite(db_path)?;
    let tx = conn
        .transaction()
        .with_context(|| format!("Failed to begin pool state transaction in {}", db_path))?;

    tx.execute(&format!("DELETE FROM {POOL_STATE_TABLE}"), [])
        .with_context(|| format!("Failed to clear pool state table in {}", db_path))?;

    let insert_sql = format!(
        "INSERT INTO {POOL_STATE_TABLE} (\
            wallet_idx,\
            wallet_private_key_hex,\
            privacy_spend_key_hex,\
            current_note_value,\
            current_note_rho_hex,\
            current_note_sender_id_hex,\
            pending_tx_hash,\
            pending_next_note_value,\
            pending_next_note_rho_hex,\
            pending_next_note_sender_id_hex\
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
    );
    let mut stmt = tx
        .prepare(&insert_sql)
        .with_context(|| format!("Failed to prepare pool state insert in {}", db_path))?;

    for (wallet_idx, wallet) in persisted.wallets.into_iter().enumerate() {
        let current_note_value = wallet
            .current_note
            .as_ref()
            .map(|note| note.value.to_string());
        let current_note_rho_hex = wallet
            .current_note
            .as_ref()
            .map(|note| note.rho_hex.as_str());
        let current_note_sender_id_hex = wallet
            .current_note
            .as_ref()
            .map(|note| note.sender_id_hex.as_str());

        let pending_tx_hash = wallet
            .pending
            .as_ref()
            .map(|pending| pending.tx_hash.as_str());
        let pending_next_note_value = wallet
            .pending
            .as_ref()
            .map(|pending| pending.next_note.value.to_string());
        let pending_next_note_rho_hex = wallet
            .pending
            .as_ref()
            .map(|pending| pending.next_note.rho_hex.as_str());
        let pending_next_note_sender_id_hex = wallet
            .pending
            .as_ref()
            .map(|pending| pending.next_note.sender_id_hex.as_str());

        stmt.execute(params![
            i64::try_from(wallet_idx).context("wallet index overflow while persisting state")?,
            wallet.wallet_private_key_hex,
            wallet.privacy_spend_key_hex,
            current_note_value.as_deref(),
            current_note_rho_hex,
            current_note_sender_id_hex,
            pending_tx_hash,
            pending_next_note_value.as_deref(),
            pending_next_note_rho_hex,
            pending_next_note_sender_id_hex
        ])
        .with_context(|| {
            format!(
                "Failed to insert persisted wallet {} into {}",
                wallet_idx, db_path
            )
        })?;
    }

    drop(stmt);
    tx.commit()
        .with_context(|| format!("Failed to commit pool state transaction in {}", db_path))?;
    Ok(())
}

fn note_from_db_columns(
    value: Option<String>,
    rho_hex: Option<String>,
    sender_id_hex: Option<String>,
    field_name: &str,
    wallet_idx: usize,
) -> Result<Option<PersistedNote>> {
    match (value, rho_hex, sender_id_hex) {
        (None, None, None) => Ok(None),
        (Some(value), Some(rho_hex), Some(sender_id_hex)) => Ok(Some(PersistedNote {
            value: value.parse::<u128>().with_context(|| {
                format!(
                    "Failed to parse {} value as u128 for wallet {}",
                    field_name, wallet_idx
                )
            })?,
            rho_hex,
            sender_id_hex,
        })),
        _ => bail!(
            "Incomplete {} columns for wallet {} in persisted state DB",
            field_name,
            wallet_idx
        ),
    }
}

fn load_pool_state_sqlite_sync(db_path: &str) -> Result<Option<PersistedPoolState>> {
    if !Path::new(db_path).exists() {
        return Ok(None);
    }

    let conn = open_pool_state_sqlite(db_path)?;
    let query_sql = format!(
        "SELECT wallet_idx, wallet_private_key_hex, privacy_spend_key_hex, \
         current_note_value, current_note_rho_hex, current_note_sender_id_hex, \
         pending_tx_hash, pending_next_note_value, pending_next_note_rho_hex, \
         pending_next_note_sender_id_hex \
         FROM {POOL_STATE_TABLE} \
         ORDER BY wallet_idx ASC"
    );
    let mut stmt = conn
        .prepare(&query_sql)
        .with_context(|| format!("Failed to prepare pool state query in {}", db_path))?;
    let mut rows = stmt
        .query([])
        .with_context(|| format!("Failed to query pool state rows from {}", db_path))?;

    let mut wallets: Vec<PersistedWallet> = Vec::new();
    while let Some(row) = rows
        .next()
        .with_context(|| format!("Failed to read row from {}", db_path))?
    {
        let wallet_idx_raw: i64 = row
            .get(0)
            .with_context(|| format!("Failed to read wallet_idx from {}", db_path))?;
        let wallet_idx = usize::try_from(wallet_idx_raw).with_context(|| {
            format!(
                "Invalid negative or overflowing wallet_idx {} in {}",
                wallet_idx_raw, db_path
            )
        })?;
        let expected_wallet_idx = wallets.len();
        if wallet_idx != expected_wallet_idx {
            bail!(
                "Corrupt pool state DB {}: expected wallet_idx {}, found {}",
                db_path,
                expected_wallet_idx,
                wallet_idx
            );
        }

        let wallet_private_key_hex: String = row.get(1).with_context(|| {
            format!(
                "Failed to read wallet_private_key_hex for wallet {} from {}",
                wallet_idx, db_path
            )
        })?;
        let privacy_spend_key_hex: String = row.get(2).with_context(|| {
            format!(
                "Failed to read privacy_spend_key_hex for wallet {} from {}",
                wallet_idx, db_path
            )
        })?;
        let current_note = note_from_db_columns(
            row.get(3).with_context(|| {
                format!(
                    "Failed to read current_note_value for wallet {} from {}",
                    wallet_idx, db_path
                )
            })?,
            row.get(4).with_context(|| {
                format!(
                    "Failed to read current_note_rho_hex for wallet {} from {}",
                    wallet_idx, db_path
                )
            })?,
            row.get(5).with_context(|| {
                format!(
                    "Failed to read current_note_sender_id_hex for wallet {} from {}",
                    wallet_idx, db_path
                )
            })?,
            "current_note",
            wallet_idx,
        )?;

        let pending_note = note_from_db_columns(
            row.get(7).with_context(|| {
                format!(
                    "Failed to read pending_next_note_value for wallet {} from {}",
                    wallet_idx, db_path
                )
            })?,
            row.get(8).with_context(|| {
                format!(
                    "Failed to read pending_next_note_rho_hex for wallet {} from {}",
                    wallet_idx, db_path
                )
            })?,
            row.get(9).with_context(|| {
                format!(
                    "Failed to read pending_next_note_sender_id_hex for wallet {} from {}",
                    wallet_idx, db_path
                )
            })?,
            "pending_next_note",
            wallet_idx,
        )?;

        let pending_tx_hash: Option<String> = row.get(6).with_context(|| {
            format!(
                "Failed to read pending_tx_hash for wallet {} from {}",
                wallet_idx, db_path
            )
        })?;
        let pending = match (pending_tx_hash, pending_note) {
            (None, None) => None,
            (Some(tx_hash), Some(next_note)) => Some(PersistedPending { tx_hash, next_note }),
            (Some(_), None) | (None, Some(_)) => {
                bail!(
                    "Incomplete pending columns for wallet {} in persisted state DB {}",
                    wallet_idx,
                    db_path
                );
            }
        };

        wallets.push(PersistedWallet {
            wallet_private_key_hex,
            privacy_spend_key_hex,
            current_note,
            pending,
        });
    }

    if wallets.is_empty() {
        return Ok(None);
    }

    Ok(Some(PersistedPoolState { wallets }))
}

async fn load_pool_state_sqlite(db_path: &str) -> Result<Option<PersistedPoolState>> {
    let db_path = db_path.to_string();
    tokio::task::spawn_blocking(move || load_pool_state_sqlite_sync(&db_path))
        .await
        .context("Pool state SQLite load task failed to join")?
}

async fn reconcile_restored_pending_with_worker_db(state: &Arc<ServiceState>) -> Result<()> {
    let pending_hashes = fetch_pending_hashes_from_verifier(state).await?;

    let mut kept_pending = 0usize;
    let mut cleared_not_pending = 0usize;

    let mut wallets = state.wallets.write().await;
    for wallet in wallets.iter_mut() {
        let Some(pending) = wallet.pending.clone() else {
            continue;
        };

        if pending_hashes.contains(&pending.tx_hash) {
            kept_pending += 1;
        } else {
            wallet.pending = None;
            cleared_not_pending += 1;
        }
    }

    let ready_after = wallets.iter().filter(|w| w.pending.is_some()).count();
    drop(wallets);

    state
        .ready_proofs_count
        .store(ready_after, Ordering::Relaxed);
    if cleared_not_pending > 0 {
        request_pool_state_save(state);
    }

    tracing::info!(
        kept_pending,
        cleared_not_pending,
        ready_after,
        "Reconciled restored pending proofs against worker_verified_transactions"
    );

    Ok(())
}

/// Restore wallets from a persisted SQLite DB.
/// Returns `Ok(true)` if wallets were restored, `Ok(false)` if no state DB found.
async fn restore_wallets_from_state(state: &Arc<ServiceState>) -> Result<bool> {
    let sqlite_path = match configured_pool_state_sqlite_path(&state.cfg) {
        Some(path) => path.to_string(),
        None => return Ok(false),
    };
    let persisted = match load_pool_state_sqlite(&sqlite_path).await? {
        Some(state) => state,
        None => return Ok(false),
    };

    if persisted.wallets.is_empty() {
        tracing::info!(sqlite_path, "Pool state DB is empty, starting fresh");
        return Ok(false);
    }

    tracing::info!(
        wallet_count = persisted.wallets.len(),
        sqlite_path,
        "Restoring wallets from persisted state"
    );

    let mut wallets = Vec::with_capacity(persisted.wallets.len());
    let mut ready_count = 0usize;

    for (idx, pw) in persisted.wallets.iter().enumerate() {
        let wallet = McpWalletContext::from_private_key_hex(&pw.wallet_private_key_hex)
            .with_context(|| format!("Failed to restore wallet {}", idx))?;
        let privacy_key = PrivacyKey::from_hex(&pw.privacy_spend_key_hex)
            .with_context(|| format!("Failed to restore privacy key {}", idx))?;

        let current_note = pw
            .current_note
            .as_ref()
            .map(NoteState::from_persisted)
            .transpose()
            .with_context(|| format!("Failed to restore current_note for wallet {}", idx))?;

        let pending = pw
            .pending
            .as_ref()
            .map(|p| {
                Ok::<_, anyhow::Error>(PendingTransfer {
                    tx_hash: p.tx_hash.clone(),
                    next_note: NoteState::from_persisted(&p.next_note)?,
                })
            })
            .transpose()
            .with_context(|| format!("Failed to restore pending for wallet {}", idx))?;

        if pending.is_some() {
            ready_count += 1;
        }

        wallets.push(PoolWallet {
            wallet,
            wallet_private_key_hex: pw.wallet_private_key_hex.clone(),
            privacy_key,
            privacy_spend_key_hex: pw.privacy_spend_key_hex.clone(),
            viewer_fvk_bundle: None,
            current_note,
            pending,
            generating: false,
        });
    }

    let wallet_count = wallets.len();
    *state.wallets.write().await = wallets;
    state
        .ready_proofs_count
        .store(ready_count, Ordering::Relaxed);

    reconcile_restored_pending_with_worker_db(state).await?;

    let ready_count_after_reconcile = ready_proofs(state);
    tracing::info!(
        wallet_count,
        ready_count = ready_count_after_reconcile,
        "Wallets restored from state DB"
    );

    Ok(true)
}

fn spawn_state_saver(state: Arc<ServiceState>) {
    tokio::spawn(async move {
        if state.cfg.pool_state_file.is_none() {
            return;
        }

        loop {
            state.state_save_notify.notified().await;

            // Debounce: coalesce rapid changes so we don't thrash disk I/O
            // under heavy proof-generation load.
            sleep(Duration::from_secs(2)).await;

            // Drain the dirty flag – if more changes landed during the
            // debounce window they are captured in this single save.
            state.state_dirty.swap(false, Ordering::AcqRel);
            if let Err(e) = save_pool_state(&state).await {
                tracing::warn!(error = %e, "Failed to save pool state");
            }
        }
    });
}

fn request_pool_state_save(state: &Arc<ServiceState>) {
    if state.cfg.pool_state_file.is_none() {
        return;
    }

    state.state_dirty.store(true, Ordering::Release);
    state.state_save_notify.notify_one();
}

fn generate_key_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn env_required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required"))
}

fn env_string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_optional_string(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_u128(key: &str, default: u128) -> u128 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u128>().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn tree_resolve_retry_attempts() -> u32 {
    std::env::var("PROOF_POOL_TREE_RESOLVE_RETRY_ATTEMPTS")
        .ok()
        .or_else(|| std::env::var("MCP_TREE_RESOLVE_RETRY_ATTEMPTS").ok())
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_TREE_RESOLVE_RETRY_ATTEMPTS)
}

fn tree_resolve_retry_delay_ms() -> u64 {
    std::env::var("PROOF_POOL_TREE_RESOLVE_RETRY_DELAY_MS")
        .ok()
        .or_else(|| std::env::var("MCP_TREE_RESOLVE_RETRY_DELAY_MS").ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TREE_RESOLVE_RETRY_DELAY_MS)
}

fn is_tree_positions_resolution_error(error_text: &str) -> bool {
    error_text.contains("Failed to resolve Merkle positions/openings from cached commitment tree")
}

fn is_invalid_anchor_root_error(error_text: &str) -> bool {
    error_text.contains("Invalid anchor root")
}
