use std::collections::HashMap;
use std::net::SocketAddr;
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
use mcp_external::ligero::Ligero;
use mcp_external::operations::{deposit, send_funds, transfer, TransferInputNote, DEFAULT_MAX_FEE};
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::provider::Provider;
use mcp_external::server::{McpSpec, McpWalletContext};
use midnight_privacy::{note_commitment, Hash32};
use rand::RngCore;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use sov_bank::TokenId;
use sov_modules_api::Amount;
use sov_proof_verifier_service::{
    create_router as create_verifier_router, AppState, ServiceConfig,
};
use sov_rollup_interface::crypto::{PrivateKey as _, PublicKey as _};
use sov_rollup_interface::zk::{CodeCommitment, Zkvm, ZkvmHost};
use tempfile::NamedTempFile;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

const DOMAIN: [u8; 32] = [1u8; 32];

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
    setup_concurrency: usize,
    max_concurrent_proofs: usize,
    da_connection_string: String,
    ligero_program_path: String,
    ligero_proof_service_url: String,
    verifier_prover_service_url: Option<String>,
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

        let setup_concurrency = env_usize("SETUP_CONCURRENCY", 10).max(1);
        let max_concurrent_proofs = env_usize("MAX_CONCURRENT_PROOFS", 5).max(1);

        let ligero_program_path = env_string("LIGERO_PROGRAM_PATH", "note_spend_guest");
        let ligero_proof_service_url =
            env_string("LIGERO_PROOF_SERVICE_URL", "http://127.0.0.1:1313");
        let verifier_prover_service_url = env_optional_string("VERIFIER_PROVER_SERVICE_URL")
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
            setup_concurrency,
            max_concurrent_proofs,
            da_connection_string,
            ligero_program_path,
            ligero_proof_service_url,
            verifier_prover_service_url,
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

#[derive(Clone)]
struct PoolWallet {
    wallet: McpWalletContext,
    privacy_key: PrivacyKey,
    viewer_fvk_bundle: Option<ViewerFvkBundle>,
    current_note: Option<NoteState>,
    pending: Option<PendingTransfer>,
    generating: bool,
}

struct ServiceState {
    cfg: Config,
    provider: Arc<Provider>,
    http: HttpClient,
    verifier_url: String,
    ligero: Arc<Ligero>,
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

#[derive(Debug, Serialize)]
struct StatusResponse {
    max_proofs: usize,
    ready_proofs: usize,
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = Config::from_env()?;

    // Avoid a 10s sequencer confirmation wait in mcp-external::operations::transfer.
    std::env::set_var("MCP_TRANSFER_WAIT_MODE", "none");

    let verifier_url = start_embedded_verifier(&cfg).await?;
    tracing::info!(verifier_url, "Embedded verifier started (defer mode)");

    wait_for_sequencer_ready(&cfg.rollup_rpc_url, Duration::from_secs(60)).await?;

    let provider = Arc::new(
        Provider::new(&cfg.rollup_rpc_url, &verifier_url, &cfg.indexer_url)
            .await
            .context("Failed to create rollup provider")?,
    );
    start_background_tree_sync(provider.clone());

    let gas_token_id = provider.get_gas_token_id().await.context("gas token id")?;

    let ligero = Arc::new(Ligero::new(
        cfg.ligero_proof_service_url.clone(),
        cfg.ligero_program_path.clone(),
    ));

    let admin_wallet = Arc::new(
        McpWalletContext::from_private_key_hex(&cfg.admin_wallet_private_key_hex)
            .context("ADMIN_WALLET_PRIVATE_KEY")?,
    );
    tracing::info!(
        admin_address = %admin_wallet.get_address(),
        max_proofs = cfg.max_proofs,
        deposit_amount = cfg.deposit_amount,
        gas_reserve = cfg.auto_fund_gas_reserve,
        "Starting proof pool service"
    );

    let state = Arc::new(ServiceState {
        cfg: cfg.clone(),
        provider: provider.clone(),
        http: HttpClient::new(),
        verifier_url: verifier_url.clone(),
        ligero: ligero.clone(),
        admin_wallet: admin_wallet.clone(),
        gas_token_id,
        wallets: RwLock::new(Vec::new()),
        proof_semaphore: Arc::new(Semaphore::new(cfg.max_concurrent_proofs)),
    });

    setup_wallets_and_fill_pool(state.clone()).await?;
    spawn_refill_loop(state.clone());

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/status", get(status_handler))
        .route("/send", post(send_handler).get(send_handler_get))
        .route("/burst", post(burst_handler).get(burst_handler_get))
        .with_state(state);

    tracing::info!(bind = %cfg.bind_addr, "HTTP server listening");
    let listener = TcpListener::bind(cfg.bind_addr)
        .await
        .context("Failed to bind proof pool service")?;

    axum::serve(
        listener,
        ServiceExt::<axum::extract::Request>::into_make_service(app),
    )
    .await
    .context("Failed to serve proof pool service")?;

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn status_handler(
    State(state): State<Arc<ServiceState>>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<StatusResponse>, StatusCode> {
    check_auth(&state.cfg.auth_token, query.auth_token.as_deref())?;
    let ready = ready_proofs(&state).await;
    Ok(Json(StatusResponse {
        max_proofs: state.cfg.max_proofs,
        ready_proofs: ready,
    }))
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

    let flush = flush_verifier(&state, Some(requested))
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    apply_flush_results(&state, &flush).await;
    let ready = ready_proofs(&state).await;

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
    let burst_start = tokio::time::Instant::now();

    let mut steps: Vec<BurstStep> = Vec::with_capacity(quantities.len());
    for (idx, requested) in quantities.iter().copied().enumerate() {
        tokio::time::sleep_until(burst_start + interval * (idx as u32)).await;

        let flush = flush_verifier(&state, Some(requested))
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;

        apply_flush_results(&state, &flush).await;
        let ready = ready_proofs(&state).await;

        steps.push(BurstStep {
            requested,
            flushed: flush.flushed,
            accepted: flush.accepted,
            rejected: flush.rejected,
            ready_proofs: ready,
        });
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

async fn ready_proofs(state: &Arc<ServiceState>) -> usize {
    let wallets = state.wallets.read().await;
    wallets.iter().filter(|w| w.pending.is_some()).count()
}

async fn apply_flush_results(state: &Arc<ServiceState>, flush: &FlushSummary) {
    let mut wallets = state.wallets.write().await;
    let mut by_hash: HashMap<String, usize> = HashMap::new();
    for (idx, w) in wallets.iter().enumerate() {
        if let Some(ref p) = w.pending {
            by_hash.insert(p.tx_hash.clone(), idx);
        }
    }

    for entry in flush.results.iter() {
        let Some(ref tx_hash) = entry.tx_hash else {
            continue;
        };
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

        if entry.accepted {
            w.current_note = Some(pending.next_note);
        }
        w.pending = None;
    }
}

fn spawn_refill_loop(state: Arc<ServiceState>) {
    tokio::spawn(async move {
        loop {
            let ready = ready_proofs(&state).await;
            if ready >= state.cfg.max_proofs {
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            let permit = match state.proof_semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };

            let wallet_idx = match pick_wallet_to_generate(&state).await {
                Some(idx) => idx,
                None => {
                    drop(permit);
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            let st = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = generate_pending_for_wallet(&st, wallet_idx).await {
                    tracing::warn!(wallet_idx, error = %e, "Failed to generate pending proof");
                }
            });
        }
    });
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

        let res = transfer(
            state.ligero.as_ref(),
            state.provider.as_ref(),
            &wallet,
            spend_sk,
            pk_ivk_owner,
            current_note.value,
            vec![input],
            destination_pk_spend,
            destination_pk_ivk,
            viewer_fvk_bundle,
        )
        .await
        .context("transfer (self)")?;

        let next_note = NoteState {
            value: current_note.value,
            rho: res.output_rho,
            sender_id: privacy_key.recipient(&DOMAIN),
        };

        {
            let mut wallets = state.wallets.write().await;
            let w = wallets
                .get_mut(wallet_idx)
                .ok_or_else(|| anyhow!("wallet idx out of range (write)"))?;
            w.pending = Some(PendingTransfer {
                tx_hash: res.tx_hash.clone(),
                next_note,
            });
            w.generating = false;
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
        let wallet = McpWalletContext::from_private_key_hex(&generate_key_hex())?;
        let privacy_key = PrivacyKey::from_hex(generate_key_hex())?;
        wallets.push(PoolWallet {
            wallet,
            privacy_key,
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

    tracing::info!("Flushing deposits to sequencer");
    let _ = flush_verifier(&state, None).await?;

    tracing::info!("Waiting for deposit notes to be indexed");
    for (wallet_idx, note) in deposit_notes.iter().enumerate() {
        let wallets = state.wallets.read().await;
        let w = wallets
            .get(wallet_idx)
            .ok_or_else(|| anyhow!("wallet idx out of range"))?;
        wait_for_note_in_tree(state.provider.as_ref(), &w.privacy_key, note).await?;
    }

    {
        let mut wallets = state.wallets.write().await;
        for (idx, note) in deposit_notes.into_iter().enumerate() {
            if let Some(w) = wallets.get_mut(idx) {
                w.current_note = Some(note);
            }
        }
    }

    tracing::info!("Generating initial pending proofs (MAX_PROOFS)");
    fill_pool_initial(state.clone()).await?;

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        ready = ready_proofs(&state).await,
        "Startup complete"
    );

    Ok(())
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

    let sem = Arc::new(Semaphore::new(state.cfg.setup_concurrency));
    let mut join_set: JoinSet<Result<(usize, ViewerFvkBundle)>> = JoinSet::new();

    let wallets = state.wallets.read().await;
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
    drop(wallets);

    let mut out: Vec<Option<ViewerFvkBundle>> = vec![None; state.cfg.max_proofs];
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
    let sem = Arc::new(Semaphore::new(state.cfg.setup_concurrency));
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();

    let wallets = state.wallets.read().await;
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
    drop(wallets);

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn wait_for_wallet_balances(state: &Arc<ServiceState>, min_balance: u128) -> Result<()> {
    let sem = Arc::new(Semaphore::new(state.cfg.setup_concurrency));
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();

    let wallets = state.wallets.read().await;
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
    drop(wallets);

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn submit_deposits(state: &Arc<ServiceState>) -> Result<Vec<NoteState>> {
    let sem = Arc::new(Semaphore::new(state.cfg.setup_concurrency));
    let mut join_set: JoinSet<Result<(usize, NoteState)>> = JoinSet::new();

    let wallets = state.wallets.read().await;
    for (idx, w) in wallets.iter().enumerate() {
        let provider = state.provider.clone();
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
    drop(wallets);

    let mut out: Vec<Option<NoteState>> = vec![None; state.cfg.max_proofs];
    while let Some(res) = join_set.join_next().await {
        let (idx, note) = res??;
        out[idx] = Some(note);
    }

    out.into_iter()
        .map(|o| o.ok_or_else(|| anyhow!("missing deposit note result")))
        .collect()
}

async fn fill_pool_initial(state: Arc<ServiceState>) -> Result<()> {
    let mut join_set: JoinSet<Result<()>> = JoinSet::new();
    for idx in 0..state.cfg.max_proofs {
        let st = state.clone();
        let permit = st.proof_semaphore.clone().acquire_owned().await?;
        join_set.spawn(async move {
            let _permit = permit;
            generate_pending_for_wallet(&st, idx).await?;
            Ok(())
        });
    }

    while let Some(res) = join_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn flush_verifier(state: &Arc<ServiceState>, limit: Option<usize>) -> Result<FlushSummary> {
    let mut url = format!(
        "{}/midnight-privacy/flush",
        state.verifier_url.trim_end_matches('/')
    );
    if let Some(limit) = limit {
        url = format!("{}?limit={}", url, limit);
    }

    let resp = state
        .http
        .post(&url)
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

fn compute_ligero_method_id(program: &str) -> Result<[u8; 32]> {
    let program_str = program.to_string();
    let host = <sov_ligero_adapter::Ligero as Zkvm>::Host::from_args(&program_str);
    let code_commitment = host.code_commitment();
    let method_id: [u8; 32] = code_commitment
        .encode()
        .try_into()
        .map_err(|_| anyhow!("code commitment should be 32 bytes"))?;
    Ok(method_id)
}

async fn start_embedded_verifier(cfg: &Config) -> Result<String> {
    let method_id = compute_ligero_method_id(&cfg.ligero_program_path)?;

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
        max_concurrent_verifications: cfg.max_concurrent_proofs,
        chain_id: 1,
        da_connection_string: cfg.da_connection_string.clone(),
        defer_sequencer_submission: true,
        prover_service_url: cfg
            .verifier_prover_service_url
            .clone()
            .or_else(|| Some(cfg.ligero_proof_service_url.clone())),
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
