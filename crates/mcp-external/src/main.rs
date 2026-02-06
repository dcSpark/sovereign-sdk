use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
    HeaderMap, Method, Request, StatusCode,
};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use rmcp::model::{ClientJsonRpcMessage, ClientNotification, InitializedNotification};
use rmcp::transport::common::http_header::HEADER_SESSION_ID;
use rmcp::transport::common::server_side_http::{session_id, SessionId};
use rmcp::transport::streamable_http_server::session::local::{
    create_local_session, LocalSessionManager, LocalSessionManagerError, LocalSessionWorker,
};
use rmcp::transport::streamable_http_server::SessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::WorkerTransport;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use url::Url;

mod commitment_tree;
mod config;
mod fvk_service;
mod ligero;
mod operations;
mod prefunded_wallets;
mod privacy_key;
mod provider;
mod server;
mod session_store;
mod viewer;
mod wallet;

#[cfg(test)]
mod test_utils;

use std::cell::RefCell;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::sync::{Mutex, RwLock};
use tracing_subscriber::prelude::*;

use crate::config::Config;
use crate::fvk_service::ViewerFvkBundle;
use crate::ligero::Ligero;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::server::{CryptoServer, LocalNotes, McpWalletContext, PendingSpentNotes};
use crate::session_store::{SessionSnapshot, SessionStore};
use crate::wallet::WalletContext;

const DEFAULT_AUTO_FUND_GAS_RESERVE: u128 = 1_000_000u128;

struct SessionContext {
    requested_id: Option<String>,
    created_id: Option<String>,
    restore_tx: Option<oneshot::Sender<()>>,
}

tokio::task_local! {
    static SESSION_CONTEXT: RefCell<SessionContext>;
}

#[derive(Debug, Default)]
struct PersistentSessionManager {
    inner: LocalSessionManager,
}

impl SessionManager for PersistentSessionManager {
    type Error = LocalSessionManagerError;
    type Transport = WorkerTransport<LocalSessionWorker>;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let requested_id = SESSION_CONTEXT
            .try_with(|ctx| ctx.borrow().requested_id.clone())
            .ok()
            .flatten();

        let mut id: SessionId = requested_id.map(Into::into).unwrap_or_else(session_id);
        if self.inner.sessions.read().await.contains_key(&id) {
            tracing::warn!(session_id = %id, "requested MCP session id already exists; generating a new id");
            id = session_id();
        }

        let (handle, worker) = create_local_session(id.clone(), self.inner.session_config.clone());
        self.inner.sessions.write().await.insert(id.clone(), handle);

        let _ = SESSION_CONTEXT.try_with(|ctx| {
            ctx.borrow_mut().created_id = Some(id.to_string());
        });

        Ok((id, WorkerTransport::spawn(worker)))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: rmcp::model::ClientJsonRpcMessage,
    ) -> Result<rmcp::model::ServerJsonRpcMessage, Self::Error> {
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: rmcp::model::ClientJsonRpcMessage,
    ) -> Result<
        impl futures::Stream<Item = rmcp::transport::common::server_side_http::ServerSseMessage>
            + Send
            + 'static,
        Self::Error,
    > {
        self.inner.create_stream(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<
        impl futures::Stream<Item = rmcp::transport::common::server_side_http::ServerSseMessage>
            + Send
            + 'static,
        Self::Error,
    > {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<
        impl futures::Stream<Item = rmcp::transport::common::server_side_http::ServerSseMessage>
            + Send
            + 'static,
        Self::Error,
    > {
        self.inner.resume(id, last_event_id).await
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: rmcp::model::ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.inner.accept_message(id, message).await
    }
}

struct McpSessions {
    service: StreamableHttpService<CryptoServer, PersistentSessionManager>,
    session_manager: Arc<PersistentSessionManager>,
    session_store: Option<Arc<SessionStore>>,
    auto_initialize_sessions: bool,
}

impl McpSessions {
    fn new(
        service: StreamableHttpService<CryptoServer, PersistentSessionManager>,
        session_manager: Arc<PersistentSessionManager>,
        session_store: Option<Arc<SessionStore>>,
        auto_initialize_sessions: bool,
    ) -> Self {
        Self {
            service,
            session_manager,
            session_store,
            auto_initialize_sessions,
        }
    }

    fn build_initialize_request_body() -> Vec<u8> {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "clientInfo": { "name": "mcp-external-auto", "version": "0.0.0" },
                "capabilities": {}
            }
        })
        .to_string()
        .into_bytes()
    }

    async fn send_initialized_notification(&self, session_id: &str) {
        if !self.auto_initialize_sessions {
            return;
        }
        let session_id: SessionId = session_id.to_string().into();
        let notification = ClientJsonRpcMessage::notification(
            ClientNotification::InitializedNotification(InitializedNotification::default()),
        );
        if let Err(err) = self
            .session_manager
            .accept_message(&session_id, notification)
            .await
        {
            tracing::warn!("Failed to auto-send initialized notification for {session_id}: {err}");
        }
    }

    async fn bootstrap_session(
        &self,
        requested_session_id: String,
    ) -> Result<String, axum::response::Response> {
        let (restore_tx, restore_rx) = if self.session_store.is_some() {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let session_context = SessionContext {
            requested_id: Some(requested_session_id),
            created_id: None,
            restore_tx,
        };

        let init_body = Self::build_initialize_request_body();
        let init_request = match Request::builder()
            .method(Method::POST)
            .uri("/")
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(init_body))
        {
            Ok(req) => req,
            Err(err) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to build initialize request: {err}"),
                )
                    .into_response());
            }
        };

        let response = SESSION_CONTEXT
            .scope(RefCell::new(session_context), async {
                self.service.handle(init_request).await.into_response()
            })
            .await;

        if let Some(rx) = restore_rx {
            if let Err(err) = rx.await {
                tracing::warn!("Session restore channel closed unexpectedly: {err}");
            }
        }

        if !response.status().is_success() {
            return Err(response);
        }

        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Initialize response missing session id",
                )
                    .into_response()
            })?;

        Ok(session_id)
    }

    async fn handle(&self, request: Request<Body>) -> axum::response::Response {
        let (mut parts, body) = request.into_parts();

        let body_bytes = match to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to read request body: {err}"),
                )
                    .into_response();
            }
        };

        let original_session_id = parts
            .headers
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let is_initialize = serde_json::from_slice::<serde_json::Value>(&body_bytes)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
            .is_some_and(|m| m == "initialize");

        let mut requested_session_id: Option<String> = None;
        let mut should_create_session = false;

        if let Some(session_id) = original_session_id {
            let as_session_id: SessionId = session_id.clone().into();
            let exists = self
                .session_manager
                .has_session(&as_session_id)
                .await
                .unwrap_or(false);
            if !exists {
                if parts.method == Method::POST && is_initialize {
                    requested_session_id = Some(session_id);
                    should_create_session = true;
                    parts.headers.remove(HEADER_SESSION_ID);
                } else if self.auto_initialize_sessions {
                    let store = match self.session_store.as_ref() {
                        Some(store) => store,
                        None => {
                            return (
                                StatusCode::UNAUTHORIZED,
                                "Unauthorized: Session not found. Send an MCP initialize request to create a new session.",
                            )
                                .into_response();
                        }
                    };

                    let has_snapshot = match store.load_session(&session_id).await {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(err) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Failed to load session snapshot: {err}"),
                            )
                                .into_response();
                        }
                    };

                    if !has_snapshot {
                        return (
                            StatusCode::UNAUTHORIZED,
                            "Unauthorized: Session not found. Send an MCP initialize request to create a new session.",
                        )
                            .into_response();
                    }

                    let bootstrapped_session_id = match self.bootstrap_session(session_id).await {
                        Ok(id) => id,
                        Err(response) => return response,
                    };
                    self.send_initialized_notification(&bootstrapped_session_id)
                        .await;
                } else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        "Unauthorized: Session not found. Send an MCP initialize request to create a new session.",
                    )
                        .into_response();
                }
            }
        } else if parts.method == Method::POST && is_initialize {
            should_create_session = true;
        }

        let request = Request::from_parts(parts, Body::from(body_bytes));

        if should_create_session {
            let (restore_tx, restore_rx) = if self.session_store.is_some() {
                let (tx, rx) = oneshot::channel();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            let session_context = SessionContext {
                requested_id: requested_session_id,
                created_id: None,
                restore_tx,
            };

            let response = SESSION_CONTEXT
                .scope(RefCell::new(session_context), async {
                    self.service.handle(request).await.into_response()
                })
                .await;

            if let Some(rx) = restore_rx {
                if let Err(err) = rx.await {
                    tracing::warn!("Session restore channel closed unexpectedly: {err}");
                }
            }

            if self.auto_initialize_sessions && response.status().is_success() {
                if let Some(session_id) = response
                    .headers()
                    .get(HEADER_SESSION_ID)
                    .and_then(|v| v.to_str().ok())
                {
                    self.send_initialized_notification(session_id).await;
                }
            }

            response
        } else {
            self.service.handle(request).await.into_response()
        }
    }
}

async fn restore_session_state(
    session_store: Arc<SessionStore>,
    session_id: String,
    wallet_context: Arc<RwLock<Option<McpWalletContext>>>,
    privacy_key: Arc<RwLock<Option<PrivacyKey>>>,
    viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
    wallet_explicitly_loaded: Arc<RwLock<bool>>,
    pending_spent_notes: Arc<Mutex<PendingSpentNotes>>,
    local_notes: Arc<Mutex<LocalNotes>>,
) -> anyhow::Result<()> {
    let snapshot = match session_store.load_session(&session_id).await? {
        Some(snapshot) => snapshot,
        None => {
            session_store
                .save_session(&session_id, &SessionSnapshot::empty())
                .await?;
            return Ok(());
        }
    };

    let mut restored_wallet_ctx: Option<McpWalletContext> = None;
    let mut restored_privacy_key: Option<PrivacyKey> = None;

    if let (Some(wallet_hex), Some(privacy_hex)) = (
        snapshot.wallet_private_key_hex.clone(),
        snapshot.privacy_spend_key_hex.clone(),
    ) {
        match McpWalletContext::from_private_key_hex(wallet_hex) {
            Ok(ctx) => restored_wallet_ctx = Some(ctx),
            Err(err) => {
                tracing::warn!("Failed to restore wallet context for session {session_id}: {err}")
            }
        }

        match PrivacyKey::from_hex(privacy_hex) {
            Ok(key) => restored_privacy_key = Some(key),
            Err(err) => {
                tracing::warn!("Failed to restore privacy key for session {session_id}: {err}")
            }
        }
    }

    let restored_viewer_fvk = match snapshot.viewer_fvk_bundle {
        Some(bundle) => match bundle.try_into_bundle() {
            Ok(bundle) => Some(bundle),
            Err(err) => {
                tracing::warn!(
                    "Failed to restore viewer FVK bundle for session {session_id}: {err}"
                );
                None
            }
        },
        None => None,
    };

    let loaded = snapshot.wallet_explicitly_loaded
        && restored_wallet_ctx.is_some()
        && restored_privacy_key.is_some();

    if snapshot.wallet_explicitly_loaded && !loaded {
        tracing::warn!(
            "Session {session_id} was marked as loaded but keys could not be restored; leaving it unlocked"
        );
    }

    *wallet_context.write().await = restored_wallet_ctx;
    *privacy_key.write().await = restored_privacy_key;
    *viewer_fvk_bundle.write().await = restored_viewer_fvk;
    *wallet_explicitly_loaded.write().await = loaded;
    {
        let mut pending = pending_spent_notes.lock().await;
        pending.by_rho.clear();
        for entry in &snapshot.pending_spent_notes {
            let inserted_at = std::time::UNIX_EPOCH
                .checked_add(std::time::Duration::from_millis(
                    entry.inserted_at_ms.max(0) as u64,
                ))
                .unwrap_or(std::time::UNIX_EPOCH);
            pending.by_rho.insert(entry.rho.clone(), inserted_at);
        }
    }
    {
        let mut local = local_notes.lock().await;
        local.by_rho.clear();
        for note in &snapshot.local_notes {
            local.by_rho.insert(note.rho.clone(), note.clone());
        }
    }

    Ok(())
}

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

    let prefunded_wallets = if let Some(path) = cfg
        .prefunded_wallets_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let store = crate::prefunded_wallets::PrefundedWalletStore::load_jsonl(path)?;
        tracing::info!(
            "[mcp] Loaded {} prefunded wallets from {:?}",
            store.len(),
            store.source_path()
        );
        if store.is_empty() {
            tracing::warn!(
                "[mcp] PREFUNDED_WALLETS_FILE is set but the file is empty; createWallet will fail when prefunded mode is enabled"
            );
        } else {
            match provider.import_prefunded_wallets(store.import_items()).await {
                Ok(summary) => tracing::info!(
                    "[mcp] Prefunded wallets imported to indexer: processed={}, inserted={}, ignored={}",
                    summary.processed,
                    summary.inserted,
                    summary.ignored
                ),
                Err(e) => tracing::warn!(
                    "[mcp] Failed to import prefunded wallets to indexer (createWallet may fail): {}",
                    e
                ),
            }
        }
        Some(Arc::new(store))
    } else {
        None
    };

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

    let session_store = if let Some(db_url) = cfg.mcp_session_db_url.as_deref() {
        if cfg.mcp_session_db_encryption_key.is_none() {
            tracing::warn!(
                "[mcp] MCP_SESSION_DB_ENCRYPTION_KEY not set; session data will be stored unencrypted"
            );
        }
        let store =
            SessionStore::connect(db_url, cfg.mcp_session_db_encryption_key.as_deref()).await?;
        tracing::info!("[mcp] MCP session persistence enabled");
        Some(Arc::new(store))
    } else {
        None
    };
    if cfg.mcp_auto_initialize_sessions {
        tracing::info!("[mcp] MCP auto-initialize sessions enabled");
    }

    tracing::info!(
        "[mcp] HTTP Streamable server binding to {}",
        cfg.mcp_server_bind_address
    );
    let provider_for_service = provider.clone();
    let admin_wallet_ctx_for_service = admin_wallet_ctx.clone();
    let ligero_for_service = ligero.clone();
    let prefunded_wallets_for_service = prefunded_wallets.clone();
    let log_path_string = log_file_path.to_string_lossy().to_string();
    let auto_fund_deposit_amount_for_service = auto_fund_deposit_amount;
    let auto_fund_gas_reserve_for_service = auto_fund_gas_reserve;
    let session_store_for_service = session_store.clone();

    let session_manager = Arc::new(PersistentSessionManager::default());
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
            let pending_spent_notes = Arc::new(Mutex::new(PendingSpentNotes::default()));
            let local_notes = Arc::new(Mutex::new(LocalNotes::default()));

            let session_id = SESSION_CONTEXT
                .try_with(|ctx| {
                    let ctx = ctx.borrow();
                    ctx.created_id.clone().or_else(|| ctx.requested_id.clone())
                })
                .ok()
                .flatten();

            let restore_tx = SESSION_CONTEXT
                .try_with(|ctx| ctx.borrow_mut().restore_tx.take())
                .ok()
                .flatten();

            if let (Some(store), Some(id)) = (session_store_for_service.clone(), session_id.clone())
            {
                let wallet_ctx_restore = wallet_ctx.clone();
                let privacy_key_restore = privacy_key.clone();
                let viewer_fvk_restore = viewer_fvk_bundle.clone();
                let wallet_loaded_restore = wallet_explicitly_loaded.clone();
                let pending_restore = pending_spent_notes.clone();
                let local_restore = local_notes.clone();
                tokio::spawn(async move {
                    if let Err(err) = restore_session_state(
                        store,
                        id.clone(),
                        wallet_ctx_restore,
                        privacy_key_restore,
                        viewer_fvk_restore,
                        wallet_loaded_restore,
                        pending_restore,
                        local_restore,
                    )
                    .await
                    {
                        tracing::warn!("Failed to restore session {id}: {err}");
                    }
                    if let Some(tx) = restore_tx {
                        let _ = tx.send(());
                    }
                });
            } else if let Some(tx) = restore_tx {
                let _ = tx.send(());
            }

            Ok(CryptoServer::new(
                provider_for_service.clone(),
                wallet_ctx,
                admin_wallet_ctx_for_service.clone(),
                ligero_for_service.clone(),
                viewer_fvk_bundle,
                privacy_key,
                prefunded_wallets_for_service.clone(),
                log_path_string.clone(),
                auto_fund_deposit_amount_for_service,
                auto_fund_gas_reserve_for_service,
                wallet_explicitly_loaded,
                session_id,
                session_store_for_service.clone(),
                pending_spent_notes,
                local_notes,
            ))
        },
        session_manager.clone(),
        Default::default(),
    );

    let mcp_sessions = Arc::new(McpSessions::new(
        service,
        session_manager,
        session_store,
        cfg.mcp_auto_initialize_sessions,
    ));

    let app_state = AppState {
        http_client: reqwest::Client::new(),
        provider: provider.clone(),
        admin_wallet_ctx: admin_wallet_ctx.clone(),
        authority_api_token: cfg.midnight_fvk_service_admin_token.clone(),
        metrics_api_url: cfg.metrics_api_url.clone(),
        mcp_sessions,
    };

    let mcp_router = Router::new().route("/", any(mcp_handler));

    let router = Router::new()
        .nest("/mcp", mcp_router)
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

async fn mcp_handler(
    State(state): State<AppState>,
    req: Request<Body>,
) -> axum::response::Response {
    state.mcp_sessions.handle(req).await
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
    mcp_sessions: Arc<McpSessions>,
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
            return (
                StatusCode::OK,
                Json(Vec::<(String, AuthorityWalletData)>::new()),
            )
                .into_response();
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
            .get_wallet_balance(
                privacy_address,
                None,
                Some(&fvk_entry.fvk),
                Some(&fvk_entry.fvk),
            )
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

        // Use privacy_address (bech32m privpool1...) as the wallet identifier
        accounts.push((privacy_address.clone(), wallet_data));
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

    let fetch =
        |window_seconds: u64| fetch_metrics_tps(&state.http_client, base_url, window_seconds);
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
                error:
                    "Authority write endpoints are disabled. Set MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN."
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
            (
                StatusCode::OK,
                Json(TxHashResponse {
                    tx_hash: res.tx_hash,
                }),
            )
                .into_response()
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
