use axum::extract::State;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use url::Url;

mod commitment_tree;
mod config;
mod fvk_service;
mod ligero;
mod operations;
mod privacy_key;
mod provider;
mod server;
mod viewer;
mod wallet;

#[cfg(test)]
mod test_utils;

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::prelude::*;

use crate::config::Config;
use crate::ligero::Ligero;
use crate::provider::Provider;
use crate::server::{CryptoServer, McpWalletContext};
use crate::wallet::WalletContext;

const DEFAULT_AUTO_FUND_GAS_RESERVE: u128 = 1_000_000u128;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env()?;
    let log_dir = std::path::PathBuf::from("logs");
    let log_file_path = log_dir.join("mcp-external.log");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::never(&log_dir, "mcp-external.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

    let fmt_stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let fmt_file = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(fmt_stderr)
        .with(fmt_file)
        .init();

    tracing::info!("[mcp] Starting Sovereign SDK MCP Server");
    tracing::info!("[mcp] Rollup RPC URL: {}", cfg.rollup_rpc_url);
    tracing::info!("[mcp] Verifier URL: {}", cfg.verifier_url);
    tracing::info!("[mcp] Indexer URL: {}", cfg.indexer_url);
    if cfg.start_with_new_wallet {
        tracing::warn!(
            "[mcp] START_WITH_NEW_WALLET is deprecated/ignored: sessions start empty. Use createWallet per MCP session."
        );
    }

    let admin_wallet_ctx = cfg
        .admin_wallet_private_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(WalletContext::from_private_key_hex)
        .transpose()?
        .map(Arc::new);

    if let Some(ref admin_wallet_ctx) = admin_wallet_ctx {
        tracing::info!(
            "[mcp] Admin wallet address (auto-fund): {}",
            admin_wallet_ctx.get_address()
        );
    }

    tracing::info!("[mcp] Connecting to rollup RPC, verifier service, and indexer...");
    let provider = Provider::new(
        cfg.rollup_rpc_url.as_str(),
        cfg.verifier_url.as_str(),
        cfg.indexer_url.as_str(),
    )
    .await?;
    tracing::info!("[mcp] Connected to rollup RPC, verifier service, and indexer successfully");
    let provider = Arc::new(provider);

    // Keep the commitment tree cache warm in the background so transfers across many wallets
    // don't all pay the sync cost on-demand.
    crate::commitment_tree::start_background_tree_sync(provider.clone());

    // Initialize Ligero proof client (HTTP service)
    tracing::info!("[mcp] Initializing Ligero proof service client");
    tracing::info!("[mcp] Proof service URL: {}", cfg.ligero_proof_service_url);
    tracing::info!("[mcp] Circuit: {}", cfg.ligero_program_path);

    let ligero = Arc::new(Ligero::new(
        cfg.ligero_proof_service_url.to_string(),
        cfg.ligero_program_path.clone(),
    ));

    let auto_fund_deposit_amount = cfg
        .auto_fund_deposit_amount
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u128>().map_err(|e| {
                tracing::warn!(
                    "[auto-fund] Invalid AUTO_FUND_DEPOSIT_AMOUNT '{}': {}",
                    s,
                    e
                );
                e
            })
        })
        .and_then(Result::ok);

    if let Some(amount) = auto_fund_deposit_amount {
        tracing::info!(
            "[auto-fund] Configured auto-fund deposit amount: {}",
            amount
        );
    } else {
        tracing::info!(
            "[auto-fund] No AUTO_FUND_DEPOSIT_AMOUNT configured; skipping auto-funding on wallet creation"
        );
    }

    let auto_fund_gas_reserve = cfg
        .auto_fund_gas_reserve
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u128>().map_err(|e| {
                tracing::warn!("[auto-fund] Invalid AUTO_FUND_GAS_RESERVE '{}': {}", s, e);
                e
            })
        })
        .and_then(Result::ok)
        .unwrap_or(DEFAULT_AUTO_FUND_GAS_RESERVE);

    if auto_fund_deposit_amount.is_some() {
        tracing::info!(
            "[auto-fund] Configured auto-fund gas reserve: {}",
            auto_fund_gas_reserve
        );
    }

    tracing::info!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );
    let provider_for_service = provider.clone();
    let admin_wallet_ctx_for_service = admin_wallet_ctx.clone();
    let ligero_for_service = ligero.clone();
    let log_path_string = log_file_path.to_string_lossy().to_string();
    let auto_fund_deposit_amount_for_service = auto_fund_deposit_amount;
    let auto_fund_gas_reserve_for_service = auto_fund_gas_reserve;

    let service = StreamableHttpService::new(
        move || {
            // Each MCP session starts with no wallet loaded and gets isolated state (no cross-talk
            // between providers). Call createWallet or restoreWallet to set per-session keys.
            let wallet_ctx = Arc::new(RwLock::new(None));
            let privacy_key = Arc::new(RwLock::new(None));
            let viewer_fvk_bundle = Arc::new(RwLock::new(None));
            // This flag is only used to prevent accidentally overwriting a wallet that was
            // created/restored via MCP tools within this session.
            // Sessions start empty and remain "unlocked" by default.
            let wallet_explicitly_loaded = Arc::new(RwLock::new(false));

            Ok(CryptoServer::new(
                provider_for_service.clone(),
                wallet_ctx,
                admin_wallet_ctx_for_service.clone(),
                ligero_for_service.clone(),
                viewer_fvk_bundle,
                privacy_key,
                log_path_string.clone(),
                auto_fund_deposit_amount_for_service,
                auto_fund_gas_reserve_for_service,
                wallet_explicitly_loaded,
            ))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let app_state = AppState {
        http_client: reqwest::Client::new(),
        provider: provider.clone(),
        admin_wallet_ctx: admin_wallet_ctx.clone(),
        authority_api_token: cfg.midnight_fvk_service_admin_token.clone(),
        metrics_api_url: cfg.metrics_api_url.clone(),
    };

    let router = Router::new()
        .nest_service("/mcp", service)
        .route("/health", get(health_handler))
        .route("/authority", get(authority_index_handler))
        .route("/authority/info", get(authority_info_handler))
        .route("/authority/accounts", get(authority_accounts_handler))
        .route("/authority/freeze", post(authority_freeze_handler))
        .route("/authority/thaw", post(authority_thaw_handler))
        .route("/authority/tps", get(authority_tps_handler))
        .with_state(app_state);
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.mcp_server_bind_address).await?;

    tracing::info!(
        "[mcp] Server started successfully! Listening on http://{}",
        cfg.mcp_server_bind_address
    );
    tracing::info!(
        "[mcp] MCP endpoint: http://{}/mcp",
        cfg.mcp_server_bind_address
    );
    tracing::info!(
        "[mcp] Health endpoint: http://{}/health",
        cfg.mcp_server_bind_address
    );
    tracing::info!(
        "[mcp] Authority endpoints: http://{}/authority/*",
        cfg.mcp_server_bind_address
    );

    let _ = axum::serve(tcp_listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("\n[mcp] Shutting down gracefully...");
        })
        .await;

    Ok(())
}

/// Health check response
#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
    service: String,
    #[serde(rename = "checkedAt")]
    checked_at: String,
}

/// Health check endpoint handler
async fn health_handler() -> impl IntoResponse {
    let response = HealthResponse {
        status: "healthy".to_string(),
        service: "mcp-external".to_string(),
        checked_at: chrono::Utc::now().to_rfc3339(),
    };
    (StatusCode::OK, Json(response))
}

#[derive(Clone)]
struct AppState {
    http_client: reqwest::Client,
    provider: Arc<Provider>,
    admin_wallet_ctx: Option<Arc<McpWalletContext>>,
    authority_api_token: Option<String>,
    metrics_api_url: Option<Url>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct AuthorityIndexResponse {
    service: String,
    endpoints: AuthorityEndpoints,
    write_enabled: bool,
}

#[derive(Debug, Serialize)]
struct AuthorityEndpoints {
    info: String,
    accounts: String,
    freeze: String,
    thaw: String,
    tps: String,
}

async fn authority_index_handler(State(state): State<AppState>) -> impl IntoResponse {
    let response = AuthorityIndexResponse {
        service: "mcp-external".to_string(),
        endpoints: AuthorityEndpoints {
            info: "/authority/info".to_string(),
            accounts: "/authority/accounts".to_string(),
            freeze: "/authority/freeze".to_string(),
            thaw: "/authority/thaw".to_string(),
            tps: "/authority/tps".to_string(),
        },
        write_enabled: state.authority_api_token.is_some() && state.admin_wallet_ctx.is_some(),
    };
    (StatusCode::OK, Json(response))
}


/// Wallet data matching MockMCP's /authority/accounts response format
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorityWalletData {
    /// Authority verification key (FVK hex)
    authority_vfk: String,
    /// Current balance as string
    balance: String,
    /// Frozen status: null if not frozen, or string with freeze reason
    frozen: Option<String>,
    /// Timestamp of last send transaction (ISO 8601)
    last_send: String,
    /// Pending balance as string
    pending_balance: String,
    /// Privacy address (bech32 format)
    privacy_address: String,
    /// Privacy spend key - always null for security (we don't expose private keys)
    privacy_spend_key: Option<String>,
}

/// Response type for /authority/accounts - array of [wallet_id, wallet_data] tuples
/// Matches MockMCP's response format
type AuthorityAccountsResponse = Vec<(String, AuthorityWalletData)>;

/// Response type for /authority/info - array of frozen wallet addresses
/// Matches MockMCP's response format
type AuthorityInfoResponse = Vec<String>;

async fn authority_info_handler(State(state): State<AppState>) -> impl IntoResponse {
    // /authority/info returns just the list of frozen addresses (matches MockMCP spec)
    match crate::operations::list_frozen_addresses(&state.provider).await {
        Ok(res) => {
            let frozen_addresses: AuthorityInfoResponse =
                res.addresses.into_iter().map(|a| a.to_string()).collect();
            (StatusCode::OK, Json(frozen_addresses)).into_response()
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: format!("Failed to fetch frozen addresses: {e}"),
            }),
        )
            .into_response(),
    }
}

async fn authority_accounts_handler(State(state): State<AppState>) -> impl IntoResponse {
    // /authority/accounts returns all accounts with wallet data (matches MockMCP spec)
    authority_accounts_full(state).await
}

async fn authority_accounts_full(state: AppState) -> impl IntoResponse {
    // Step 1: Get all registered FVKs from the indexer
    let fvk_registry = match state.provider.get_fvk_registry().await {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!("Failed to fetch FVK registry: {}", e);
            // Return empty array if indexer is unavailable
            return (StatusCode::OK, Json(Vec::<(String, AuthorityWalletData)>::new())).into_response();
        }
    };

    // Step 2: Get frozen addresses to check freeze status
    let frozen_addresses: std::collections::HashSet<String> =
        match crate::operations::list_frozen_addresses(&state.provider).await {
            Ok(res) => res.addresses.into_iter().map(|a| a.to_string()).collect(),
            Err(e) => {
                tracing::warn!("Failed to fetch frozen addresses: {}", e);
                std::collections::HashSet::new()
            }
        };

    // Step 3: Build account list with balances
    let mut accounts: AuthorityAccountsResponse = Vec::new();

    for fvk_entry in fvk_registry.fvks {
        let Some(ref privacy_address) = fvk_entry.shielded_address else {
            // Skip FVKs without associated addresses
            continue;
        };

        // Try to get balance for this address
        let (balance, last_send) = match state
            .provider
            .get_wallet_balance(privacy_address, None, Some(&fvk_entry.fvk), Some(&fvk_entry.fvk))
            .await
        {
            Ok(balance_resp) => {
                // Find the most recent transfer (for lastSend timestamp)
                let last_send_ts = balance_resp
                    .unspent_notes
                    .iter()
                    .filter(|n| n.kind == "transfer")
                    .map(|n| n.timestamp_ms)
                    .max();

                let last_send = match last_send_ts {
                    Some(ts) => chrono::DateTime::from_timestamp_millis(ts)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_else(|| "0000-01-01T00:00:00Z".to_string()),
                    None => "0000-01-01T00:00:00Z".to_string(),
                };

                (balance_resp.balance, last_send)
            }
            Err(e) => {
                tracing::debug!(
                    "Failed to fetch balance for {}: {}",
                    &privacy_address[..20.min(privacy_address.len())],
                    e
                );
                ("0".to_string(), "0000-01-01T00:00:00Z".to_string())
            }
        };

        // Check if address is frozen
        let frozen = if frozen_addresses.contains(privacy_address) {
            Some("Frozen by authority".to_string())
        } else {
            None
        };

        let wallet_data = AuthorityWalletData {
            authority_vfk: fvk_entry.fvk,
            balance,
            frozen,
            last_send,
            pending_balance: "0".to_string(), // Not tracked in our system
            privacy_address: privacy_address.clone(),
            privacy_spend_key: None, // Never expose private keys
        };

        // Use wallet_address (sov1...) as the identifier if available, otherwise fall back to privacy_address
        let wallet_id = fvk_entry
            .wallet_address
            .clone()
            .unwrap_or_else(|| privacy_address.clone());
        accounts.push((wallet_id, wallet_data));
    }

    (StatusCode::OK, Json(accounts)).into_response()
}

fn is_authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|provided| provided == token)
}

fn extract_privacy_address(body: &serde_json::Value) -> Option<String> {
    match body {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => arr.get(0).and_then(|v| v.as_str()).map(|s| s.to_string()),
        serde_json::Value::Object(map) => map
            .get("privacyAddress")
            .or_else(|| map.get("privacy_address"))
            .or_else(|| map.get("address"))
            .or_else(|| map.get("walletAddress"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

#[derive(Debug, Serialize)]
struct TxHashResponse {
    tx_hash: String,
}

#[derive(Debug, Deserialize)]
struct MetricsTpsResponse {
    tps: Option<f64>,
}

#[derive(Debug, Serialize)]
struct AuthorityTpsResponse {
    #[serde(rename = "lastTick")]
    last_tick: String,
    m1: Option<f64>,
    m5: Option<f64>,
    m15: Option<f64>,
}

async fn authority_tps_handler(State(state): State<AppState>) -> impl IntoResponse {
    let Some(base_url) = state.metrics_api_url.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Metrics API not configured. Set METRICS_API_URL (sov-metrics-api base URL) to enable `/authority/tps`."
                    .to_string(),
            }),
        )
            .into_response();
    };

    let fetch = |window_seconds: u64| fetch_metrics_tps(&state.http_client, base_url, window_seconds);
    let (m1, m5, m15) = tokio::join!(fetch(60), fetch(300), fetch(900));

    let (m1, m5, m15) = match (m1, m5, m15) {
        (Ok(m1), Ok(m5), Ok(m15)) => (m1, m5, m15),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: format!("Failed to fetch TPS from metrics API: {e}"),
                }),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(AuthorityTpsResponse {
            last_tick: chrono::Utc::now().to_rfc3339(),
            m1,
            m5,
            m15,
        }),
    )
        .into_response()
}

async fn fetch_metrics_tps(
    http: &reqwest::Client,
    base_url: &Url,
    window_seconds: u64,
) -> Result<Option<f64>, String> {
    let mut url = base_url
        .join("/tps")
        .map_err(|e| format!("Invalid METRICS_API_URL: {e}"))?;
    url.set_query(Some(&format!("window_seconds={window_seconds}")));

    let resp = http
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status} from {url}: {body}"));
    }

    let parsed: MetricsTpsResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(parsed.tps)
}

async fn authority_freeze_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    authority_set_frozen(state, headers, body, true).await
}

async fn authority_thaw_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    authority_set_frozen(state, headers, body, false).await
}

async fn authority_set_frozen(
    state: AppState,
    headers: HeaderMap,
    body: serde_json::Value,
    freeze: bool,
) -> axum::response::Response {
    let Some(ref token) = state.authority_api_token else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Authority write endpoints are disabled. Set MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN."
                    .to_string(),
            }),
        )
            .into_response();
    };

    if !is_authorized(&headers, token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Unauthorized".to_string(),
            }),
        )
            .into_response();
    }

    let Some(admin_wallet_ctx) = state.admin_wallet_ctx.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Authority write endpoints require ADMIN_WALLET_PRIVATE_KEY.".to_string(),
            }),
        )
            .into_response();
    };

    let addr_str = match extract_privacy_address(&body) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Missing privacy address in request body.".to_string(),
                }),
            )
                .into_response()
        }
    };

    // Extract reason from body (second element of array, or "reason" field in object)
    let reason = extract_freeze_reason(&body);

    let addr: midnight_privacy::PrivacyAddress = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid privacy address: {e}"),
                }),
            )
                .into_response()
        }
    };

    let res = if freeze {
        crate::operations::freeze_address(&state.provider, admin_wallet_ctx.as_ref(), addr).await
    } else {
        crate::operations::unfreeze_address(&state.provider, admin_wallet_ctx.as_ref(), addr).await
    };

    match res {
        Ok(res) => {
            // Record the freeze event in the indexer
            let action = if freeze { "freeze" } else { "unfreeze" };
            if let Err(e) = record_freeze_event_to_indexer(
                &state.provider,
                &addr_str,
                reason.as_deref(),
                freeze,
                Some(&res.tx_hash),
            )
            .await
            {
                tracing::warn!(
                    "Failed to record {} event to indexer for {}: {}",
                    action,
                    &addr_str[..20.min(addr_str.len())],
                    e
                );
            }
            (StatusCode::OK, Json(TxHashResponse { tx_hash: res.tx_hash })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Failed to submit transaction: {e}"),
            }),
        )
            .into_response(),
    }
}

fn extract_freeze_reason(body: &serde_json::Value) -> Option<String> {
    match body {
        serde_json::Value::Array(arr) => {
            // Get second element as reason
            arr.get(1).and_then(|v| v.as_str()).map(|s| s.to_string())
        }
        serde_json::Value::Object(map) => map
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

async fn record_freeze_event_to_indexer(
    provider: &crate::provider::Provider,
    privacy_address: &str,
    reason: Option<&str>,
    is_frozen: bool,
    tx_hash: Option<&str>,
) -> anyhow::Result<()> {
    let indexer_url = provider.indexer_url();
    let endpoint = format!("{}/frozen", indexer_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "privacy_address": privacy_address,
        "reason": reason,
        "is_frozen": is_frozen,
        "tx_hash": tx_hash,
    });

    let http = reqwest::Client::new();
    let resp = http
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to call indexer: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Indexer returned {}: {}", status, body);
    }

    Ok(())
}
