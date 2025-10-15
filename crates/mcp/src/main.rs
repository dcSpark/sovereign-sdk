use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpService,
};

mod config;
mod ligero;
mod operations;
mod server;
mod wallet;

use crate::config::Config;
use crate::server::CryptoServer;
use crate::wallet::WalletContext;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load typed + validated configuration from the environment (with `.env`).
    let cfg = Config::from_env()?;

    eprintln!("[mcp] Starting Sovereign SDK MCP Server");
    eprintln!("[mcp] Rollup RPC URL: {}", cfg.rollup_rpc_url);
    eprintln!("[mcp] Loading wallet from: {}", cfg.wallet_path.display());

    // Load wallet context - try to connect to RPC, but don't fail if it's unavailable
    let ctx = match WalletContext::load(&cfg.wallet_path, Some(cfg.rollup_rpc_url.as_str())).await {
        Ok(ctx) => {
            eprintln!("[mcp] Wallet loaded successfully");
            eprintln!("[mcp] Connected to rollup RPC");
            ctx
        }
        Err(e) => {
            // If RPC connection fails, try loading wallet without RPC
            if e.to_string().contains("Failed to connect to node") {
                eprintln!("[mcp] Warning: Could not connect to rollup RPC: {}", e);
                eprintln!("[mcp] Loading wallet without RPC connection...");
                WalletContext::load(&cfg.wallet_path, None).await?
            } else {
                // For other errors (like wallet loading), fail
                return Err(e.into());
            }
        }
    };

    if let Some(addr) = ctx.default_address() {
        eprintln!("[mcp] Default wallet address: {}", addr.address);
    }
    let wallet_ctx = Arc::new(RwLock::new(ctx));

    eprintln!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );

    // Create streamable HTTP service with local session manager
    let service = StreamableHttpService::new(
        move || Ok(CryptoServer::with_wallet(wallet_ctx.clone())),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    // Create Axum router and nest the service at the base path
    let router = axum::Router::new().nest_service("/mcp", service);

    // Bind to the configured address
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.mcp_server_bind_address).await?;

    eprintln!(
        "[mcp] Server started successfully! Listening on http://{}",
        cfg.mcp_server_bind_address
    );
    eprintln!(
        "[mcp] MCP endpoint: http://{}/mcp",
        cfg.mcp_server_bind_address
    );

    // Serve with graceful shutdown on Ctrl+C
    let _ = axum::serve(tcp_listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            eprintln!("\n[mcp] Shutting down gracefully...");
        })
        .await;

    Ok(())
}
