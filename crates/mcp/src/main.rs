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
mod viewer;
mod wallet;

#[cfg(test)]
mod test_utils;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::authority_vfk::AuthorityVfk;
use crate::config::Config;
use crate::ligero::Ligero;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::server::CryptoServer;
use crate::wallet::WalletContext;

const DOMAIN: [u8; 32] = [1u8; 32];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
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
    tracing::info!(
        "[mcp] Prover binary: {}",
        cfg.ligero_prover_binary_path.display()
    );
    tracing::info!("[mcp] Shader path: {}", cfg.ligero_shader_path.display());
    tracing::info!("[mcp] Program path: {}", cfg.ligero_program_path.display());

    let ligero = Arc::new(Ligero::new(
        Some(cfg.ligero_prover_binary_path.clone()),
        Some(cfg.ligero_shader_path.clone()),
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
    tracing::info!("[mcp] Privacy address: {}", privacy_key.privacy_address(&DOMAIN));
    tracing::info!(
        "[mcp] All deposits will be made to this privacy address: {}",
        privacy_key.privacy_address(&DOMAIN)
    );

    let privacy_key = Arc::new(RwLock::new(privacy_key));

    tracing::info!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );
    let service = StreamableHttpService::new(
        move || {
            Ok(CryptoServer::new(
                provider.clone(),
                wallet_ctx.clone(),
                ligero.clone(),
                authority_vfk.clone(),
                privacy_key.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.mcp_server_bind_address).await?;

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
