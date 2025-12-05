use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use tracing_subscriber::EnvFilter;

mod authority_fvk;
mod config;
mod ligero;
mod operations;
mod provider;
mod server;
mod wallet;

#[cfg(test)]
mod test_utils;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::authority_fvk::AuthorityFvk;
use crate::config::Config;
use crate::ligero::Ligero;
use crate::provider::Provider;
use crate::server::CryptoServer;
use crate::wallet::WalletContext;

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

    // Create wallet from private key hex string (no files needed!)
    let wallet_ctx = WalletContext::from_private_key_hex(&cfg.wallet_private_key)?;
    let wallet_address = wallet_ctx.get_address();
    tracing::info!("[mcp] Wallet address: {}", wallet_address);
    let wallet_ctx = Arc::new(RwLock::new(wallet_ctx));

    // Initialize RPC provider (separate from wallet)
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
        None, // verifier not needed for MCP server
        Some(cfg.ligero_shader_path.clone()),
        Some(cfg.ligero_program_path.clone()),
    ));

    // Initialize authority FVK if provided
    let authority_fvk = if let Some(ref fvk_hex) = cfg.authority_fvk {
        tracing::info!("[mcp] Initializing authority FVK from environment variable");
        match AuthorityFvk::from_hex(fvk_hex) {
            Ok(fvk) => {
                tracing::info!("[mcp] Authority FVK initialized successfully");
                Some(Arc::new(fvk))
            }
            Err(e) => {
                tracing::warn!("[mcp] Failed to initialize authority FVK: {}", e);
                tracing::warn!("[mcp] Note decryption will not be available");
                None
            }
        }
    } else {
        tracing::info!("[mcp] No AUTHORITY_FVK provided, note decryption will not be available");
        None
    };

    tracing::info!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );

    // Create streamable HTTP service with local session manager
    let service = StreamableHttpService::new(
        move || {
            Ok(CryptoServer::new(
                provider.clone(),
                wallet_ctx.clone(),
                ligero.clone(),
                authority_fvk.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    // Create Axum router and nest the service at the base path
    let router = axum::Router::new().nest_service("/mcp", service);

    // Bind to the configured address
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.mcp_server_bind_address).await?;

    tracing::info!(
        "[mcp] Server started successfully! Listening on http://{}",
        cfg.mcp_server_bind_address
    );
    tracing::info!(
        "[mcp] MCP endpoint: http://{}/mcp",
        cfg.mcp_server_bind_address
    );

    // Serve with graceful shutdown on Ctrl+C
    let _ = axum::serve(tcp_listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("\n[mcp] Shutting down gracefully...");
        })
        .await;

    Ok(())
}
