use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use tracing_subscriber::EnvFilter;

mod config;
mod ligero;
mod operations;
mod server;
mod wallet;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::ligero::Ligero;
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
    tracing::info!("[mcp] Loading wallet from: {}", cfg.wallet_path.display());

    // Load wallet context - try to connect to RPC, but don't fail if it's unavailable
    let ctx = match WalletContext::load(&cfg.wallet_path, Some(cfg.rollup_rpc_url.as_str())).await {
        Ok(ctx) => {
            tracing::info!("[mcp] Wallet loaded successfully");
            tracing::info!("[mcp] Connected to rollup RPC");
            ctx
        }
        Err(e) => {
            // If RPC connection fails, try loading wallet without RPC
            if e.to_string().contains("Failed to connect to node") {
                tracing::info!("[mcp] Warning: Could not connect to rollup RPC: {}", e);
                tracing::info!("[mcp] Loading wallet without RPC connection...");
                WalletContext::load(&cfg.wallet_path, None).await?
            } else {
                // For other errors (like wallet loading), fail
                return Err(e.into());
            }
        }
    };

    if let Some(addr) = ctx.default_address() {
        tracing::info!("[mcp] Default wallet address: {}", addr.address);
    }
    let wallet_ctx = Arc::new(RwLock::new(ctx));

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

    tracing::info!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );

    // Create streamable HTTP service with local session manager
    let service = StreamableHttpService::new(
        move || {
            Ok(CryptoServer::with_wallet(
                wallet_ctx.clone(),
                ligero.clone(),
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
