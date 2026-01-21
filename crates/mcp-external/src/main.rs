use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use serde::Serialize;
use tracing_subscriber::EnvFilter;

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
use crate::fvk_service::{fetch_viewer_fvk_bundle, parse_hex_32, ViewerFvkBundle};
use crate::ligero::Ligero;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::server::CryptoServer;
use crate::wallet::WalletContext;

const DOMAIN: [u8; 32] = [1u8; 32];
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
    tracing::info!("[mcp] Initializing wallet from private key...");

    let wallet_ctx = WalletContext::from_private_key_hex(&cfg.wallet_private_key)?;
    let wallet_address = wallet_ctx.get_address();
    tracing::info!("[mcp] Wallet address: {}", wallet_address);
    let wallet_ctx = Arc::new(RwLock::new(wallet_ctx));

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

    // Initialize Ligero proof client (HTTP service)
    tracing::info!("[mcp] Initializing Ligero proof service client");
    tracing::info!(
        "[mcp] Proof service URL: {}",
        cfg.ligero_proof_service_url
    );
    tracing::info!("[mcp] Circuit: {}", cfg.ligero_program_path);

    let ligero = Arc::new(Ligero::new(
        cfg.ligero_proof_service_url.to_string(),
        cfg.ligero_program_path.clone(),
    ));

    let pool_fvk_pk = std::env::var("POOL_FVK_PK")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| parse_hex_32("POOL_FVK_PK", &s))
        .transpose()?;

    let viewer_fvk_bundle: Option<ViewerFvkBundle> = if pool_fvk_pk.is_some() {
        let http = reqwest::Client::new();
        let base_url = crate::fvk_service::fvk_service_base_url_from_env();
        tracing::info!(
            "[mcp] POOL_FVK_PK set: fetching viewer FVK bundle from midnight-fvk-service ({base_url})"
        );
        Some(fetch_viewer_fvk_bundle(&http, pool_fvk_pk).await?)
    } else {
        tracing::info!("[mcp] POOL_FVK_PK not set: viewer FVK bundle disabled");
        None
    };

    let viewer_fvk_bundle = Arc::new(RwLock::new(viewer_fvk_bundle));

    tracing::info!("[mcp] Initializing privacy key from PRIVPOOL_SPEND_KEY");

    let privacy_key = if cfg.privpool_spend_key.starts_with("privpool1") {
        PrivacyKey::from_address(&cfg.privpool_spend_key)
    } else {
        PrivacyKey::from_hex(&cfg.privpool_spend_key)
    }
    .map_err(|e| {
        format!(
            "Failed to initialize privacy key from PRIVPOOL_SPEND_KEY: {}. \
            Please provide a valid 32-byte hex string (with or without 0x prefix) \
            or a bech32m privacy address (privpool1...)",
            e
        )
    })?;

    tracing::info!("[mcp] Privacy key initialized successfully");
    tracing::info!(
        "[mcp] Privacy address: {}",
        privacy_key.privacy_address(&DOMAIN)
    );
    tracing::info!(
        "[mcp] All deposits will be made to this privacy address: {}",
        privacy_key.privacy_address(&DOMAIN)
    );

    let privacy_key = Arc::new(RwLock::new(privacy_key));

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
                tracing::warn!(
                    "[auto-fund] Invalid AUTO_FUND_GAS_RESERVE '{}': {}",
                    s,
                    e
                );
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
    let wallet_ctx_for_service = wallet_ctx.clone();
    let admin_wallet_ctx_for_service = admin_wallet_ctx.clone();
    let ligero_for_service = ligero.clone();
    let viewer_fvk_bundle_for_service = viewer_fvk_bundle.clone();
    let privacy_key_for_service = privacy_key.clone();
    let log_path_string = log_file_path.to_string_lossy().to_string();
    let auto_fund_deposit_amount_for_service = auto_fund_deposit_amount;
    let auto_fund_gas_reserve_for_service = auto_fund_gas_reserve;

    // Track whether a wallet has been loaded (including from environment variables)
    // Starts as true since the initial wallet is loaded from environment variables
    let wallet_explicitly_loaded = Arc::new(RwLock::new(true));
    let wallet_explicitly_loaded_for_service = wallet_explicitly_loaded.clone();

    let service = StreamableHttpService::new(
        move || {
            Ok(CryptoServer::new(
                provider_for_service.clone(),
                wallet_ctx_for_service.clone(),
                admin_wallet_ctx_for_service.clone(),
                ligero_for_service.clone(),
                viewer_fvk_bundle_for_service.clone(),
                privacy_key_for_service.clone(),
                log_path_string.clone(),
                auto_fund_deposit_amount_for_service,
                auto_fund_gas_reserve_for_service,
                wallet_explicitly_loaded_for_service.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .route("/health", axum::routing::get(health_handler));
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
