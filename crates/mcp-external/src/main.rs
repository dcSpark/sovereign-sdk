use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use tracing_subscriber::EnvFilter;

mod authority_vfk;
mod config;
mod ligero;
mod operations;
mod privacy_key;
mod provider;
mod server;
mod tx_store;
mod viewer;
mod wallet;

#[cfg(test)]
mod test_utils;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing_subscriber::prelude::*;

use crate::authority_vfk::AuthorityVfk;
use crate::config::Config;
use crate::ligero::Ligero;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::server::sync_with_indexer_impl;
use crate::server::CryptoServer;
use crate::tx_store::TransactionStore;
use crate::wallet::WalletContext;

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

    tracing::info!("[mcp] Connecting to rollup RPC, verifier service, and indexer...");
    let provider = Provider::new(
        cfg.rollup_rpc_url.as_str(),
        cfg.verifier_url.as_str(),
        cfg.indexer_url.as_str(),
    )
    .await?;
    tracing::info!("[mcp] Connected to rollup RPC, verifier service, and indexer successfully");
    let provider = Arc::new(provider);

    // Initialize Ligero prover
    tracing::info!("[mcp] Initializing Ligero prover");
    if let Some(ref prover) = cfg.ligero_prover_binary_path {
        tracing::info!("[mcp] Prover binary (override): {}", prover.display());
    } else {
        tracing::info!("[mcp] Prover binary: <auto-discovery>");
    }
    if let Some(ref shader) = cfg.ligero_shader_path {
        tracing::info!("[mcp] Shader path (override): {}", shader.display());
    } else {
        tracing::info!("[mcp] Shader path: <auto-discovery>");
    }
    tracing::info!("[mcp] Program: {}", cfg.ligero_program_path);

    let ligero = Arc::new(Ligero::new(
        cfg.ligero_prover_binary_path.clone(),
        cfg.ligero_shader_path.clone(),
        Some(cfg.ligero_program_path.clone()),
    ));

    let authority_vfk = if let Some(ref vfk_hex) = cfg.authority_vfk {
        tracing::info!("[mcp] Initializing authority VFK from environment variable");
        match AuthorityVfk::from_hex(vfk_hex) {
            Ok(vfk) => {
                tracing::info!("[mcp] Authority VFK initialized successfully");
                Some(vfk)
            }
            Err(e) => {
                tracing::warn!("[mcp] Failed to initialize authority VFK: {}", e);
                tracing::warn!("[mcp] Note decryption will not be available");
                None
            }
        }
    } else {
        tracing::info!("[mcp] No AUTHORITY_VFK provided, note decryption will not be available");
        None
    };

    let authority_vfk = Arc::new(RwLock::new(authority_vfk));

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
    tracing::info!("[mcp] Privacy address: {}", privacy_key.privacy_address());
    tracing::info!(
        "[mcp] All deposits will be made to this privacy address: {}",
        privacy_key.privacy_address()
    );

    let privacy_key = Arc::new(RwLock::new(privacy_key));

    // In-memory transaction store
    let tx_store = Arc::new(TransactionStore::new_in_memory().await?);

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

    tracing::info!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );
    let provider_for_service = provider.clone();
    let wallet_ctx_for_service = wallet_ctx.clone();
    let ligero_for_service = ligero.clone();
    let authority_vfk_for_service = authority_vfk.clone();
    let privacy_key_for_service = privacy_key.clone();
    let tx_store_for_service = tx_store.clone();
    let log_path_string = log_file_path.to_string_lossy().to_string();
    let auto_fund_deposit_amount_for_service = auto_fund_deposit_amount;

    let service = StreamableHttpService::new(
        move || {
            Ok(CryptoServer::new(
                provider_for_service.clone(),
                wallet_ctx_for_service.clone(),
                ligero_for_service.clone(),
                authority_vfk_for_service.clone(),
                privacy_key_for_service.clone(),
                tx_store_for_service.clone(),
                log_path_string.clone(),
                auto_fund_deposit_amount_for_service,
            ))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.mcp_server_bind_address).await?;

    // Background sync loop to keep the in-memory DB aligned with the indexer
    {
        let sync_provider = provider.clone();
        let sync_wallet = wallet_ctx.clone();
        let sync_privacy_key = privacy_key.clone();
        let sync_store = tx_store.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs(30);
            loop {
                {
                    let ctx_guard = sync_wallet.read().await;
                    let privacy_guard = sync_privacy_key.read().await;
                    if let Err(e) = sync_with_indexer_impl(
                        &sync_provider,
                        &*ctx_guard,
                        &*privacy_guard,
                        &sync_store,
                    )
                    .await
                    {
                        tracing::warn!("Background sync failed: {}", e.message);
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    tracing::info!(
        "[mcp] Server started successfully! Listening on http://{}",
        cfg.mcp_server_bind_address
    );
    tracing::info!(
        "[mcp] MCP endpoint: http://{}/mcp",
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
