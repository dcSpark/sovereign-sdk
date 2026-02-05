use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use rmcp::model::{CallToolRequestParam, ErrorCode, JsonObject, RawContent};
use rmcp::service::ServiceExt as _;
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use rmcp::transport::StreamableHttpClientTransport;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "mcp-external-stress")]
struct Args {
    /// MCP streamable HTTP endpoint (e.g. http://host:3000/mcp). If `/mcp` is missing, it is appended.
    #[arg(long, env = "MCP_ENDPOINT")]
    mcp_endpoint: String,

    /// Only connect and list tools (no wallet creation, no transactions).
    #[arg(long, env = "MCP_STRESS_PROBE_ONLY", default_value_t = false)]
    probe: bool,

    /// Number of concurrent MCP sessions (one wallet per session).
    #[arg(long, env = "MCP_STRESS_WALLETS", default_value_t = 1)]
    wallets: usize,

    /// Amount (in dust) to send to self per transaction.
    #[arg(long, env = "MCP_STRESS_SEND_AMOUNT", default_value_t = 1)]
    send_amount: u128,

    /// Run duration. Use 0 for "until Ctrl-C".
    #[arg(long, env = "MCP_STRESS_DURATION_SECS", default_value_t = 60)]
    duration_secs: u64,

    /// Per-wallet delay between sends (rate limiting).
    #[arg(long, env = "MCP_STRESS_PER_WALLET_DELAY_MS", default_value_t = 0)]
    per_wallet_delay_ms: u64,

    /// If set, poll `getTransactionStatus` for each sent tx (adds extra load).
    #[arg(long, env = "MCP_STRESS_CONFIRM", default_value_t = false)]
    confirm: bool,

    /// Max sends per wallet session (optional).
    #[arg(long, env = "MCP_STRESS_MAX_TXS")]
    max_txs: Option<u64>,

    /// Initial wait timeout for a newly created wallet to have >0 privacy balance.
    #[arg(
        long,
        env = "MCP_STRESS_WALLET_READY_TIMEOUT_SECS",
        default_value_t = 120
    )]
    wallet_ready_timeout_secs: u64,

    /// Poll interval while waiting for wallet to have balance.
    #[arg(long, env = "MCP_STRESS_WALLET_READY_POLL_MS", default_value_t = 500)]
    wallet_ready_poll_ms: u64,

    /// Number of consecutive walletBalance requests per readiness cycle before sleeping.
    #[arg(
        long,
        env = "MCP_STRESS_WALLET_READY_BURST_REQUESTS",
        default_value_t = 3
    )]
    wallet_ready_burst_requests: u32,

    /// Per-call timeout for walletBalance while waiting for initial wallet readiness.
    #[arg(
        long,
        env = "MCP_STRESS_WALLET_READY_CALL_TIMEOUT_MS",
        default_value_t = 1500
    )]
    wallet_ready_call_timeout_ms: u64,

    /// Poll interval when confirmation is enabled.
    #[arg(long, env = "MCP_STRESS_CONFIRM_POLL_MS", default_value_t = 500)]
    confirm_poll_ms: u64,

    /// Confirmation wait timeout per tx when `--confirm` is enabled.
    #[arg(long, env = "MCP_STRESS_CONFIRM_TIMEOUT_SECS", default_value_t = 120)]
    confirm_timeout_secs: u64,

    /// How often to print the periodic progress line (set to 0 to disable).
    #[arg(long, env = "MCP_STRESS_REPORT_INTERVAL_SECS", default_value_t = 0)]
    report_interval_secs: u64,

    /// Print the periodic progress line even if nothing changed since the previous print.
    #[arg(long, env = "MCP_STRESS_REPORT_UNCHANGED", default_value_t = false)]
    report_unchanged: bool,

    /// Optional file storing stable MCP session IDs (one per line) to reuse across runs.
    ///
    /// If this is set and `--wallets N` exceeds the number of IDs in the file, new UUIDv4 values
    /// are generated and appended.
    #[arg(
        long,
        env = "MCP_STRESS_SESSION_IDS_FILE",
        default_value = ".mcp-external-stress-session-ids.txt"
    )]
    session_ids_file: PathBuf,
}

struct Counters {
    wallets_ready: AtomicU64,
    sends_ok: AtomicU64,
    sends_err: AtomicU64,
    confirms_ok: AtomicU64,
    confirms_err: AtomicU64,
    send_latency_us_total: AtomicU64,
    send_latency_us_max: AtomicU64,
    confirm_latency_us_total: AtomicU64,
    confirm_latency_us_max: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            wallets_ready: AtomicU64::new(0),
            sends_ok: AtomicU64::new(0),
            sends_err: AtomicU64::new(0),
            confirms_ok: AtomicU64::new(0),
            confirms_err: AtomicU64::new(0),
            send_latency_us_total: AtomicU64::new(0),
            send_latency_us_max: AtomicU64::new(0),
            confirm_latency_us_total: AtomicU64::new(0),
            confirm_latency_us_max: AtomicU64::new(0),
        }
    }
}

#[derive(Clone)]
struct PinnedSessionHttpClient {
    inner: reqwest::Client,
    pinned_session_id: Arc<str>,
}

impl PinnedSessionHttpClient {
    fn new(inner: reqwest::Client, pinned_session_id: Arc<str>) -> Self {
        Self {
            inner,
            pinned_session_id,
        }
    }
}

impl StreamableHttpClient for PinnedSessionHttpClient {
    type Error = reqwest::Error;

    fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
    ) -> impl std::future::Future<
        Output = std::result::Result<
            rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
           + '_ {
        let pinned = self.pinned_session_id.clone();
        let session_id = session_id.or_else(|| Some(pinned.clone()));
        async move {
            let response = StreamableHttpClient::post_message(
                &self.inner,
                uri,
                message,
                session_id,
                auth_header,
            )
            .await?;
            Ok(match response {
                rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Accepted => {
                    rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Accepted
                }
                rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Json(
                    message,
                    session_id,
                ) => rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Json(
                    message,
                    session_id.or_else(|| Some(pinned.to_string())),
                ),
                rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Sse(
                    stream,
                    session_id,
                ) => rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Sse(
                    stream,
                    session_id.or_else(|| Some(pinned.to_string())),
                ),
            })
        }
    }

    fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_header: Option<String>,
    ) -> impl std::future::Future<
        Output = std::result::Result<
            (),
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
           + '_ {
        async move { Ok(()) }
    }

    fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
    ) -> impl std::future::Future<
        Output = std::result::Result<
            futures::stream::BoxStream<
                'static,
                std::result::Result<
                    sse_stream::Sse,
                    rmcp::transport::streamable_http_client::SseError,
                >,
            >,
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
           + '_ {
        StreamableHttpClient::get_stream(&self.inner, uri, session_id, last_event_id, auth_header)
    }
}

fn normalize_mcp_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with("/mcp") {
        return Ok(trimmed.to_string());
    }
    if trimmed.contains("/mcp/") {
        return Err(anyhow!(
            "mcp endpoint looks invalid (contains '/mcp/' in the middle): {endpoint}"
        ));
    }
    Ok(format!("{trimmed}/mcp"))
}

fn parse_session_ids_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn ensure_session_ids_file(path: &Path, required: usize) -> Result<Vec<String>> {
    let existing = match std::fs::read_to_string(path) {
        Ok(contents) => parse_session_ids_file(&contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).with_context(|| format!("read session ids file {path:?}")),
    };

    if existing.len() >= required {
        return Ok(existing.into_iter().take(required).collect());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create session ids dir {parent:?}"))?;
    }

    let mut ids = existing;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open session ids file for append {path:?}"))?;

    while ids.len() < required {
        let id = Uuid::new_v4().to_string();
        use std::io::Write as _;
        writeln!(file, "{id}").with_context(|| format!("append session id to {path:?}"))?;
        ids.push(id);
    }

    Ok(ids)
}

fn first_text_content(result: &rmcp::model::CallToolResult) -> Result<&str> {
    for content in &result.content {
        match &content.raw {
            RawContent::Text(t) => return Ok(t.text.as_str()),
            _ => continue,
        }
    }
    Err(anyhow!("tool response had no text content"))
}

async fn wallet_privacy_address_opt(
    client: &rmcp::service::Peer<rmcp::service::RoleClient>,
) -> Result<Option<String>> {
    let res = client
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("walletAddress"),
            arguments: Some(rmcp::object!({})),
        })
        .await;

    let result = match res {
        Ok(ok) => ok,
        Err(rmcp::service::ServiceError::McpError(err))
            if err.code == ErrorCode::INVALID_PARAMS
                && err.message.contains("No wallet loaded") =>
        {
            return Ok(None);
        }
        Err(e) => {
            return Err(anyhow!(e)).with_context(|| "call_tool walletAddress failed");
        }
    };

    if result.is_error.unwrap_or(false) {
        return Err(anyhow!("tool walletAddress returned is_error=true"));
    }

    let text = first_text_content(&result).with_context(|| "tool walletAddress response")?;
    let json: serde_json::Value = serde_json::from_str(text)
        .with_context(|| "tool walletAddress response was not valid JSON text")?;
    let privacy_address = json
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("walletAddress response missing address"))?;
    Ok(Some(privacy_address.to_string()))
}

async fn call_tool_json(
    client: &rmcp::service::Peer<rmcp::service::RoleClient>,
    name: &'static str,
    arguments: Option<JsonObject>,
) -> Result<serde_json::Value> {
    let result = client
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed(name),
            arguments,
        })
        .await
        .map_err(|e| anyhow!(e))
        .with_context(|| format!("call_tool {name} failed"))?;

    if result.is_error.unwrap_or(false) {
        return Err(anyhow!("tool {name} returned is_error=true"));
    }

    let text = first_text_content(&result).with_context(|| format!("tool {name} response"))?;
    let json: serde_json::Value = serde_json::from_str(text)
        .with_context(|| format!("tool {name} response was not valid JSON text"))?;
    Ok(json)
}

async fn create_wallet_opt(
    client: &rmcp::service::Peer<rmcp::service::RoleClient>,
) -> Result<Option<(String, String)>> {
    let res = client
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("createWallet"),
            arguments: Some(rmcp::object!({})),
        })
        .await;

    let result = match res {
        Ok(ok) => ok,
        Err(rmcp::service::ServiceError::McpError(err))
            if err.code == ErrorCode::INVALID_PARAMS
                && err
                    .message
                    .contains("A wallet is already loaded. Call removeWallet first") =>
        {
            return Ok(None);
        }
        Err(e) => return Err(anyhow!(e)).with_context(|| "call_tool createWallet failed"),
    };

    if result.is_error.unwrap_or(false) {
        return Err(anyhow!("tool createWallet returned is_error=true"));
    }

    let text = first_text_content(&result).with_context(|| "tool createWallet response")?;
    let json: serde_json::Value = serde_json::from_str(text)
        .with_context(|| "tool createWallet response was not valid JSON text")?;

    let wallet_address = json
        .get("wallet_address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("createWallet response missing wallet_address"))?;
    let privacy_address = json
        .get("privacy_address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("createWallet response missing privacy_address"))?;

    Ok(Some((
        wallet_address.to_string(),
        privacy_address.to_string(),
    )))
}

async fn wait_for_wallet_balance(
    client: &rmcp::service::Peer<rmcp::service::RoleClient>,
    timeout: Duration,
    poll: Duration,
    burst_requests: u32,
    per_call_timeout: Duration,
) -> Result<u128> {
    let start = Instant::now();
    let mut consecutive_errors: u32 = 0;
    let mut total_errors: u64 = 0;
    let burst_requests = burst_requests.max(1);

    loop {
        for burst_attempt in 0..burst_requests {
            match tokio::time::timeout(
                per_call_timeout,
                call_tool_json(client, "walletBalance", Some(rmcp::object!({}))),
            )
            .await
            {
                Ok(Ok(json)) => {
                    consecutive_errors = 0;
                    let balance = json
                        .get("balance")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("walletBalance response missing balance"))?
                        .parse::<u128>()
                        .context("walletBalance balance is not a valid u128")?;
                    if balance > 0 {
                        return Ok(balance);
                    }
                }
                Ok(Err(e)) => {
                    consecutive_errors += 1;
                    total_errors += 1;
                    if consecutive_errors == 1 || consecutive_errors % 5 == 0 {
                        tracing::warn!(
                            consecutive_errors,
                            total_errors,
                            burst_attempt = burst_attempt + 1,
                            burst_requests,
                            waited_ms = start.elapsed().as_millis(),
                            error = format!("{:#}", e),
                            "walletBalance call failed, will retry until timeout"
                        );
                    }
                }
                Err(_) => {
                    consecutive_errors += 1;
                    total_errors += 1;
                    if consecutive_errors == 1 || consecutive_errors % 5 == 0 {
                        tracing::warn!(
                            consecutive_errors,
                            total_errors,
                            burst_attempt = burst_attempt + 1,
                            burst_requests,
                            waited_ms = start.elapsed().as_millis(),
                            per_call_timeout_ms = per_call_timeout.as_millis(),
                            "walletBalance call timed out, will retry until timeout"
                        );
                    }
                }
            }

            if start.elapsed() >= timeout {
                break;
            }
        }

        if start.elapsed() >= timeout {
            if total_errors > 0 {
                return Err(anyhow!(
                    "wallet did not reach non-zero privacy balance within {:?} (walletBalance errors: total={}, consecutive={}, burst_requests={}, per_call_timeout={:?})",
                    timeout,
                    total_errors,
                    consecutive_errors,
                    burst_requests,
                    per_call_timeout
                ));
            }
            return Err(anyhow!(
                "wallet did not reach non-zero privacy balance within {:?}",
                timeout
            ));
        }

        if poll.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(poll).await;
        }
    }
}

async fn send_to_self(
    client: &rmcp::service::Peer<rmcp::service::RoleClient>,
    destination_privacy_address: &str,
    amount: u128,
) -> Result<String> {
    let json = call_tool_json(
        client,
        "send",
        Some(rmcp::object!({
            "destinationAddress": destination_privacy_address,
            "amount": amount.to_string(),
        })),
    )
    .await?;

    let id = json
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send response missing id"))?;
    Ok(id.to_string())
}

async fn wait_for_tx_confirmed(
    client: &rmcp::service::Peer<rmcp::service::RoleClient>,
    tx_id: &str,
    poll: Duration,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let res = client
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("getTransactionStatus"),
                arguments: Some(rmcp::object!({ "transactionId": tx_id })),
            })
            .await;

        match res {
            Ok(ok) => {
                if ok.is_error.unwrap_or(false) {
                    return Err(anyhow!("getTransactionStatus returned is_error=true"));
                }
                // We don't need to parse deeply here; successful response implies it exists.
                return Ok(());
            }
            Err(rmcp::service::ServiceError::McpError(err)) => {
                if err.code == ErrorCode::INVALID_PARAMS
                    && err.message.contains("Transaction not found")
                {
                    // Not indexed yet.
                } else {
                    return Err(anyhow!(
                        "getTransactionStatus failed: code={} message={}",
                        err.code.0,
                        err.message
                    ));
                }
            }
            Err(e) => return Err(anyhow!("getTransactionStatus failed: {e}")),
        }

        if start.elapsed() >= timeout {
            return Err(anyhow!("tx did not confirm within {:?}", timeout));
        }
        tokio::time::sleep(poll).await;
    }
}

async fn wallet_worker(
    idx: usize,
    args: Arc<Args>,
    counters: Arc<Counters>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    session_id: String,
) -> Result<()> {
    let client =
        PinnedSessionHttpClient::new(reqwest::Client::default(), session_id.clone().into());
    let mut config =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            args.mcp_endpoint.clone(),
        );
    config.allow_stateless = false;
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let client = ().serve(transport).await.map_err(|e| anyhow!(e))?;

    let (wallet_address, privacy_address) = match wallet_privacy_address_opt(&client).await {
        Ok(Some(addr)) => ("<reused>".to_string(), addr),
        Ok(None) => match create_wallet_opt(&client)
            .await
            .with_context(|| format!("wallet[{idx}] createWallet"))?
        {
            Some((wallet_address, privacy_address)) => (wallet_address, privacy_address),
            None => {
                let addr = wallet_privacy_address_opt(&client)
                    .await
                    .with_context(|| format!("wallet[{idx}] walletAddress (after already-loaded)"))?
                    .ok_or_else(|| {
                        anyhow!(
                            "wallet is already loaded but walletAddress still reports no wallet"
                        )
                    })?;
                ("<reused>".to_string(), addr)
            }
        },
        Err(err) => {
            tracing::warn!(
                "wallet[{idx}] walletAddress failed; attempting createWallet anyway: {err:#}"
            );

            match create_wallet_opt(&client).await.with_context(|| {
                format!("wallet[{idx}] createWallet (after walletAddress error)")
            })? {
                Some((wallet_address, privacy_address)) => (wallet_address, privacy_address),
                None => {
                    let addr = wallet_privacy_address_opt(&client)
                        .await
                        .with_context(|| {
                            format!("wallet[{idx}] walletAddress (after already-loaded)")
                        })?
                        .ok_or_else(|| {
                            anyhow!(
                                "wallet is already loaded but walletAddress still reports no wallet"
                            )
                        })?;
                    ("<reused>".to_string(), addr)
                }
            }
        }
    };

    let _ = wait_for_wallet_balance(
        &client,
        Duration::from_secs(args.wallet_ready_timeout_secs),
        Duration::from_millis(args.wallet_ready_poll_ms),
        args.wallet_ready_burst_requests,
        Duration::from_millis(args.wallet_ready_call_timeout_ms),
    )
    .await
    .with_context(|| format!("wallet[{idx}] waiting for initial balance"))?;

    counters.wallets_ready.fetch_add(1, Ordering::Relaxed);
    tracing::debug!("wallet[{idx}] wallet_address={wallet_address}");
    tracing::info!("wallet[{idx}] ready: privacy_address={privacy_address}");

    let per_wallet_delay = Duration::from_millis(args.per_wallet_delay_ms);
    let confirm_poll = Duration::from_millis(args.confirm_poll_ms);
    let confirm_timeout = Duration::from_secs(args.confirm_timeout_secs);

    let mut stop_rx = stop_rx;
    let mut sent_attempts: u64 = 0;
    loop {
        if *stop_rx.borrow() {
            break;
        }

        if let Some(max_txs) = args.max_txs {
            if sent_attempts >= max_txs {
                break;
            }
        }
        sent_attempts += 1;
        if sent_attempts == 1 {
            tracing::info!(
                "wallet[{idx}] round=1 mapping: session_id={} wallet_address={} privacy_address={}",
                session_id,
                wallet_address,
                privacy_address
            );
        }

        let send_started = Instant::now();
        let send_res = send_to_self(&client, &privacy_address, args.send_amount).await;
        let send_elapsed = send_started.elapsed();

        let send_elapsed_us = send_elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        counters
            .send_latency_us_total
            .fetch_add(send_elapsed_us, Ordering::Relaxed);
        counters
            .send_latency_us_max
            .fetch_max(send_elapsed_us, Ordering::Relaxed);

        match send_res {
            Ok(tx_id) => {
                counters.sends_ok.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    "wallet[{idx}] sent tx_id={tx_id} elapsed_ms={}",
                    send_elapsed.as_millis()
                );
                if args.confirm {
                    let confirm_started = Instant::now();
                    let confirm_res =
                        wait_for_tx_confirmed(&client, &tx_id, confirm_poll, confirm_timeout).await;
                    let confirm_elapsed = confirm_started.elapsed();

                    let confirm_elapsed_us =
                        confirm_elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
                    counters
                        .confirm_latency_us_total
                        .fetch_add(confirm_elapsed_us, Ordering::Relaxed);
                    counters
                        .confirm_latency_us_max
                        .fetch_max(confirm_elapsed_us, Ordering::Relaxed);

                    match confirm_res {
                        Ok(()) => {
                            counters.confirms_ok.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                "wallet[{idx}] confirmed tx_id={tx_id} elapsed_ms={}",
                                confirm_elapsed.as_millis()
                            );
                        }
                        Err(e) => {
                            counters.confirms_err.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                "wallet[{idx}] confirm failed elapsed_ms={} error={e:#}",
                                confirm_elapsed.as_millis()
                            );
                        }
                    }
                }
            }
            Err(e) => {
                counters.sends_err.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "wallet[{idx}] send failed elapsed_ms={} error={e:#}",
                    send_elapsed.as_millis()
                );
            }
        }

        if !per_wallet_delay.is_zero() {
            tokio::select! {
                _ = stop_rx.changed() => {},
                _ = tokio::time::sleep(per_wallet_delay) => {},
            }
        }
    }

    let _ = tokio::time::timeout(Duration::from_secs(10), client.cancel()).await;
    Ok(())
}

fn avg_us(total_us: u64, count: u64) -> Option<u64> {
    if count == 0 {
        None
    } else {
        Some(total_us / count)
    }
}

async fn probe_once(mcp_endpoint: &str) -> Result<()> {
    let transport = StreamableHttpClientTransport::from_uri(mcp_endpoint.to_string());
    let client = ().serve(transport).await.map_err(|e| anyhow!(e))?;

    if let Some(info) = client.peer_info() {
        tracing::info!(
            "connected: name={:?} version={:?}",
            info.server_info.name,
            info.server_info.version
        );
    } else {
        tracing::info!("connected: peer_info not available");
    }

    let tools = client.list_all_tools().await.map_err(|e| anyhow!(e))?;
    tracing::info!("tools: {}", tools.len());
    for tool in tools {
        tracing::info!("tool: {}", tool.name);
    }

    let _ = tokio::time::timeout(Duration::from_secs(10), client.cancel()).await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let mcp_endpoint = normalize_mcp_endpoint(&args.mcp_endpoint)?;
    let args = Arc::new(Args {
        mcp_endpoint,
        ..args
    });

    if args.probe {
        probe_once(&args.mcp_endpoint).await?;
        return Ok(());
    }

    if args.wallets == 0 {
        return Err(anyhow!("--wallets must be >= 1"));
    }

    let session_ids = ensure_session_ids_file(args.session_ids_file.as_path(), args.wallets)?;

    tracing::info!(
        "starting: endpoint={} wallets={} send_amount={} duration_secs={} confirm={} wallet_ready_timeout_secs={} wallet_ready_poll_ms={} wallet_ready_burst_requests={} wallet_ready_call_timeout_ms={} session_ids_file={}",
        args.mcp_endpoint,
        args.wallets,
        args.send_amount,
        args.duration_secs,
        args.confirm,
        args.wallet_ready_timeout_secs,
        args.wallet_ready_poll_ms,
        args.wallet_ready_burst_requests,
        args.wallet_ready_call_timeout_ms,
        args.session_ids_file.display()
    );

    let counters = Arc::new(Counters::new());

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stop_task_args = args.clone();
    let wallets_total = args.wallets as u64;

    tokio::spawn(async move {
        if stop_task_args.duration_secs > 0 {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = tokio::time::sleep(Duration::from_secs(stop_task_args.duration_secs)) => {},
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = stop_tx.send(true);
    });

    let reporter_counters = counters.clone();
    let mut reporter_stop_rx = stop_rx.clone();
    let report_interval_secs = args.report_interval_secs;
    let report_unchanged = args.report_unchanged;
    if report_interval_secs > 0 {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(report_interval_secs.max(1)));

            let mut last_ready = u64::MAX;
            let mut last_sends_ok = u64::MAX;
            let mut last_sends_err = u64::MAX;
            let mut last_confirms_ok = u64::MAX;
            let mut last_confirms_err = u64::MAX;

            loop {
                tokio::select! {
                    _ = reporter_stop_rx.changed() => {
                        break;
                    }
                    _ = interval.tick() => {
                        let ready = reporter_counters.wallets_ready.load(Ordering::Relaxed);
                        let sends_ok = reporter_counters.sends_ok.load(Ordering::Relaxed);
                        let sends_err = reporter_counters.sends_err.load(Ordering::Relaxed);
                        let confirms_ok = reporter_counters.confirms_ok.load(Ordering::Relaxed);
                        let confirms_err = reporter_counters.confirms_err.load(Ordering::Relaxed);

                        let changed = ready != last_ready
                            || sends_ok != last_sends_ok
                            || sends_err != last_sends_err
                            || confirms_ok != last_confirms_ok
                            || confirms_err != last_confirms_err;

                        if !changed && !report_unchanged {
                            continue;
                        }

                        let send_us_total =
                            reporter_counters.send_latency_us_total.load(Ordering::Relaxed);
                        let send_us_max = reporter_counters.send_latency_us_max.load(Ordering::Relaxed);
                        let send_avg_us = avg_us(send_us_total, sends_ok + sends_err);

                        let confirm_us_total =
                            reporter_counters.confirm_latency_us_total.load(Ordering::Relaxed);
                        let confirm_us_max =
                            reporter_counters.confirm_latency_us_max.load(Ordering::Relaxed);
                        let confirm_count = confirms_ok + confirms_err;
                        let confirm_avg_us = avg_us(confirm_us_total, confirm_count);

                        let delta_ok = if last_sends_ok == u64::MAX {
                            0
                        } else {
                            sends_ok.saturating_sub(last_sends_ok)
                        };
                        let delta_err = if last_sends_err == u64::MAX {
                            0
                        } else {
                            sends_err.saturating_sub(last_sends_err)
                        };

                        last_ready = ready;
                        last_sends_ok = sends_ok;
                        last_sends_err = sends_err;
                        last_confirms_ok = confirms_ok;
                        last_confirms_err = confirms_err;

                        eprintln!(
                            "ready={ready}/{wallets} sends_ok={sends_ok} (+{delta_ok}/{interval}s) sends_err={sends_err} (+{delta_err}/{interval}s) send_avg_ms={send_avg_ms} send_max_ms={send_max_ms} confirms_ok={confirms_ok} confirms_err={confirms_err} confirm_avg_ms={confirm_avg_ms} confirm_max_ms={confirm_max_ms}",
                            wallets = wallets_total,
                            interval = report_interval_secs.max(1),
                            send_avg_ms = send_avg_us.map(|v| v as f64 / 1000.0).unwrap_or(0.0),
                            send_max_ms = send_us_max as f64 / 1000.0,
                            confirm_avg_ms = confirm_avg_us.map(|v| v as f64 / 1000.0).unwrap_or(0.0),
                            confirm_max_ms = confirm_us_max as f64 / 1000.0,
                        );
                    }
                }
            }
        });
    }

    let mut join_set = tokio::task::JoinSet::new();
    for idx in 0..args.wallets {
        let session_id = session_ids[idx].clone();
        join_set.spawn(wallet_worker(
            idx,
            args.clone(),
            counters.clone(),
            stop_rx.clone(),
            session_id,
        ));
    }

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("worker failed: {e:#}");
            }
            Err(e) => {
                tracing::error!("worker task join error: {e}");
            }
        }
    }

    let ready = counters.wallets_ready.load(Ordering::Relaxed);
    let sends_ok = counters.sends_ok.load(Ordering::Relaxed);
    let sends_err = counters.sends_err.load(Ordering::Relaxed);
    let confirms_ok = counters.confirms_ok.load(Ordering::Relaxed);
    let confirms_err = counters.confirms_err.load(Ordering::Relaxed);
    let send_us_total = counters.send_latency_us_total.load(Ordering::Relaxed);
    let send_us_max = counters.send_latency_us_max.load(Ordering::Relaxed);
    let send_avg_us = avg_us(send_us_total, sends_ok + sends_err);
    let confirm_us_total = counters.confirm_latency_us_total.load(Ordering::Relaxed);
    let confirm_us_max = counters.confirm_latency_us_max.load(Ordering::Relaxed);
    let confirm_avg_us = avg_us(confirm_us_total, confirms_ok + confirms_err);

    eprintln!(
        "done: ready={ready} sends_ok={sends_ok} sends_err={sends_err} send_avg_ms={send_avg_ms} send_max_ms={send_max_ms} confirms_ok={confirms_ok} confirms_err={confirms_err} confirm_avg_ms={confirm_avg_ms} confirm_max_ms={confirm_max_ms}",
        send_avg_ms = send_avg_us.map(|v| v as f64 / 1000.0).unwrap_or(0.0),
        send_max_ms = send_us_max as f64 / 1000.0,
        confirm_avg_ms = confirm_avg_us.map(|v| v as f64 / 1000.0).unwrap_or(0.0),
        confirm_max_ms = confirm_us_max as f64 / 1000.0,
    );

    Ok(())
}
