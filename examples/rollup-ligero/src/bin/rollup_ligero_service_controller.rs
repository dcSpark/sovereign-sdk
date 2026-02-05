use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use sysinfo::{Disks, System};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::signal;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::time::{timeout, Duration};

/// Maximum number of log lines to keep in the buffer for new connections
const LOG_BUFFER_SIZE: usize = 1000;

/// Broadcast channel capacity
const BROADCAST_CAPACITY: usize = 256;

fn env_flag(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub timestamp: String,
    pub stream: String, // "stdout" or "stderr"
    pub content: String,
}

impl LogLine {
    fn to_json(&self) -> String {
        serde_json::json!({
            "timestamp": self.timestamp,
            "stream": self.stream,
            "content": self.content
        })
        .to_string()
    }
}

struct ServiceState {
    child: Option<tokio::process::Child>,
    process_group: Option<i32>,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            child: None,
            process_group: None,
        }
    }
}

impl ServiceState {
    fn is_running(&mut self) -> bool {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.child = None;
                }
                Ok(None) => return true,
                Err(err) => {
                    eprintln!("Failed to check service status: {err}");
                    self.child = None;
                }
            }
        }

        if let Some(pgid) = self.process_group {
            if process_group_running(pgid) {
                return true;
            }
            self.process_group = None;
        }

        false
    }
}

struct LogBuffer {
    lines: Vec<LogLine>,
}

impl LogBuffer {
    fn new() -> Self {
        Self {
            lines: Vec::with_capacity(LOG_BUFFER_SIZE),
        }
    }

    fn push(&mut self, line: LogLine) {
        if self.lines.len() >= LOG_BUFFER_SIZE {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }

    fn get_all(&self) -> Vec<LogLine> {
        self.lines.clone()
    }

    fn clear(&mut self) {
        self.lines.clear();
    }
}

struct AppState {
    run_all_path: PathBuf,
    run_all_dir: PathBuf,
    demo_data_dir: PathBuf,
    state: Mutex<ServiceState>,
    log_tx: broadcast::Sender<LogLine>,
    log_buffer: Arc<Mutex<LogBuffer>>,
    sys_info: RwLock<System>,
}

/// System statistics response
#[derive(Debug, Serialize)]
pub struct SystemStats {
    pub cpu: CpuStats,
    pub memory: MemoryStats,
    pub disks: Vec<DiskStats>,
    pub uptime_seconds: u64,
    pub load_average: LoadAverage,
}

#[derive(Debug, Serialize)]
pub struct CpuStats {
    pub usage_percent: f32,
    pub core_count: usize,
    pub per_core_usage: Vec<f32>,
}

#[derive(Debug, Serialize)]
pub struct MemoryStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f32,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct DiskStats {
    pub name: String,
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub usage_percent: f32,
}

#[derive(Debug, Serialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

type ApiResult = Result<String, ApiError>;

/// Health status for a single service
#[derive(Debug, Clone, Serialize)]
pub struct ServiceHealth {
    pub name: String,
    pub url: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub response_time_ms: Option<f64>,
}

/// Overall health response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub services: Vec<ServiceHealth>,
    #[serde(rename = "checkedAt")]
    pub checked_at: String,
}

/// Service definition for health checking
struct ServiceDefinition {
    name: &'static str,
    env_var: &'static str,
    default_url: &'static str,
    health_path: &'static str,
    /// If true, the service is optional (e.g., fvk-service only when POOL_FVK_PK is set)
    optional_env: Option<&'static str>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let script_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let run_all_path = script_dir.join("run_all.sh");
    let demo_data_dir = script_dir.join("demo_data");

    let bind_addr =
        std::env::var("SERVICE_CONTROLLER_BIND").unwrap_or_else(|_| "127.0.0.1:9090".to_string());

    let (log_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

    // Initialize system info
    let mut sys = System::new_all();
    sys.refresh_all();

    let app_state = Arc::new(AppState {
        run_all_dir: script_dir,
        run_all_path,
        demo_data_dir,
        state: Mutex::new(ServiceState::default()),
        log_tx,
        log_buffer: Arc::new(Mutex::new(LogBuffer::new())),
        sys_info: RwLock::new(sys),
    });

    if !app_state.run_all_path.exists() {
        anyhow::bail!("Missing run_all.sh at {}", app_state.run_all_path.display());
    }

    let app = Router::new()
        .route("/start", post(start).get(start))
        .route("/stop", post(stop).get(stop))
        .route("/restart", post(restart).get(restart))
        .route("/clean", post(clean).get(clean))
        .route("/health", get(health_check))
        .route("/stats", get(system_stats))
        .route("/logs", get(logs_websocket))
        .route("/logs/history", get(logs_history))
        .with_state(app_state.clone());

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to bind to {bind_addr}"))?;
    println!("Service controller listening on http://{bind_addr}");
    println!("WebSocket logs available at ws://{bind_addr}/logs");

    if env_flag("SERVICE_CONTROLLER_AUTO_START") {
        match start(State(app_state.clone())).await {
            Ok(message) => println!("{message}"),
            Err(err) => eprintln!(
                "Auto-start requested via SERVICE_CONTROLLER_AUTO_START, but failed: {}",
                err.message
            ),
        }
    }

    // Run the server with graceful shutdown on SIGTERM/SIGINT
    let shutdown_state = app_state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_state))
        .await?;

    // Final cleanup after server stops
    println!("Service controller shutting down, stopping child services...");
    if let Err(e) = shutdown_services(&app_state).await {
        eprintln!("Error during shutdown cleanup: {e}");
    }

    Ok(())
}

/// Wait for shutdown signal (SIGTERM or SIGINT) and stop services
async fn shutdown_signal(app_state: Arc<AppState>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            println!("\nReceived SIGINT (Ctrl+C), initiating shutdown...");
        }
        _ = terminate => {
            println!("\nReceived SIGTERM, initiating shutdown...");
        }
    }

    // Stop services before the server shuts down
    if let Err(e) = shutdown_services(&app_state).await {
        eprintln!("Error stopping services during shutdown: {e}");
    }
}

/// Gracefully stop all running services
async fn shutdown_services(app: &Arc<AppState>) -> Result<(), String> {
    let mut state = app.state.lock().await;
    if !state.is_running() {
        println!("No services running, nothing to stop.");
        return Ok(());
    }

    println!("Stopping child services...");

    let pid = state.child.as_ref().and_then(|child| child.id());
    let pgid = state.process_group;

    // Send SIGTERM first
    if let Some(pgid) = pgid {
        if let Err(e) = send_signal_to_group_sync(pgid, "-TERM") {
            eprintln!("Warning: failed to send SIGTERM to process group: {e}");
        }
    } else if let Some(pid) = pid {
        if let Err(e) = send_signal_sync(pid, "-TERM") {
            eprintln!("Warning: failed to send SIGTERM to process: {e}");
        }
    }

    // Wait for graceful shutdown
    let stopped = if let Some(child) = state.child.as_mut() {
        match timeout(Duration::from_secs(10), child.wait()).await {
            Ok(Ok(_)) => true,
            Ok(Err(err)) => {
                eprintln!("Error waiting for child process: {err}");
                false
            }
            Err(_) => false,
        }
    } else if let Some(pgid) = pgid {
        wait_for_group_exit(pgid, Duration::from_secs(10)).await
    } else {
        true
    };

    if stopped {
        state.child = None;
        state.process_group = None;
        println!("Services stopped gracefully.");
        return Ok(());
    }

    // Force kill if still running
    println!("Services did not stop gracefully, sending SIGKILL...");
    if let Some(pgid) = pgid {
        let _ = send_signal_to_group_sync(pgid, "-KILL");
    } else if let Some(pid) = pid {
        let _ = send_signal_sync(pid, "-KILL");
    }

    if let Some(child) = state.child.as_mut() {
        let _ = child.wait().await;
    } else if let Some(pgid) = pgid {
        let _ = wait_for_group_exit(pgid, Duration::from_secs(5)).await;
    }

    state.child = None;
    state.process_group = None;
    println!("Services stopped (forced).");
    Ok(())
}

/// Synchronous signal sending for use during shutdown
fn send_signal_sync(pid: u32, signal: &str) -> Result<(), String> {
    let status = std::process::Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .map_err(|e| format!("Failed to send {signal} to {pid}: {e}"))?;

    if !status.success() {
        eprintln!("kill {signal} {pid} exited with status {status}");
    }
    Ok(())
}

/// Synchronous signal sending to process group for use during shutdown
fn send_signal_to_group_sync(pgid: i32, signal: &str) -> Result<(), String> {
    let status = std::process::Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pgid}"))
        .status()
        .map_err(|e| format!("Failed to send {signal} to group {pgid}: {e}"))?;

    if !status.success() {
        eprintln!("kill {signal} -- -{pgid} exited with status {status}");
    }
    Ok(())
}

async fn start(State(app): State<Arc<AppState>>) -> ApiResult {
    let mut state = app.state.lock().await;
    if state.is_running() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Services already running",
        ));
    }

    // Clear log buffer on fresh start
    {
        let mut buffer = app.log_buffer.lock().await;
        buffer.clear();
    }

    let mut command = Command::new("bash");
    command
        .arg(&app.run_all_path)
        .current_dir(&app.run_all_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|err| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to start services: {err}"),
        )
    })?;

    let pid = child.id();

    // Take stdout and stderr for streaming
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    state.child = Some(child);
    #[cfg(unix)]
    {
        state.process_group = pid.map(|pid| pid as i32);
    }
    #[cfg(not(unix))]
    {
        state.process_group = None;
    }

    // Spawn tasks to read stdout and stderr and broadcast to WebSocket clients
    if let Some(stdout) = stdout {
        let log_tx = app.log_tx.clone();
        let log_buffer = Arc::clone(&app.log_buffer);
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let log_line = LogLine {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    stream: "stdout".to_string(),
                    content: line.clone(),
                };
                // Also print to controller's stdout
                println!("{}", line);
                // Add to buffer
                {
                    let mut buffer = log_buffer.lock().await;
                    buffer.push(log_line.clone());
                }
                // Broadcast to WebSocket clients (ignore errors if no receivers)
                let _ = log_tx.send(log_line);
            }
        });
    }

    if let Some(stderr) = stderr {
        let log_tx = app.log_tx.clone();
        let log_buffer = Arc::clone(&app.log_buffer);
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let log_line = LogLine {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    stream: "stderr".to_string(),
                    content: line.clone(),
                };
                // Also print to controller's stderr
                eprintln!("{}", line);
                // Add to buffer
                {
                    let mut buffer = log_buffer.lock().await;
                    buffer.push(log_line.clone());
                }
                // Broadcast to WebSocket clients (ignore errors if no receivers)
                let _ = log_tx.send(log_line);
            }
        });
    }

    let message = match pid {
        Some(pid) => format!("Services starting (pid {pid})"),
        None => "Services starting".to_string(),
    };
    Ok(message)
}

async fn stop(State(app): State<Arc<AppState>>) -> ApiResult {
    stop_services(&app).await
}

async fn restart(State(app): State<Arc<AppState>>) -> ApiResult {
    match stop_services(&app).await {
        Ok(_) => {}
        Err(err) if err.status == StatusCode::CONFLICT => {}
        Err(err) => return Err(err),
    }
    start(State(app)).await
}

async fn clean(State(app): State<Arc<AppState>>) -> ApiResult {
    let mut state = app.state.lock().await;
    if state.is_running() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Services must be stopped before cleaning",
        ));
    }
    drop(state);

    // Also clear log buffer
    {
        let mut buffer = app.log_buffer.lock().await;
        buffer.clear();
    }

    if app.demo_data_dir.exists() {
        std::fs::remove_dir_all(&app.demo_data_dir).map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to remove {}: {err}", app.demo_data_dir.display()),
            )
        })?;
        Ok(format!(
            "Removed database directory {}",
            app.demo_data_dir.display()
        ))
    } else {
        Ok(format!(
            "Database directory {} does not exist",
            app.demo_data_dir.display()
        ))
    }
}

/// WebSocket endpoint for streaming logs
async fn logs_websocket(
    ws: WebSocketUpgrade,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_logs_socket(socket, app))
}

async fn handle_logs_socket(socket: WebSocket, app: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // First, send all buffered logs
    {
        let buffer = app.log_buffer.lock().await;
        for log_line in buffer.get_all() {
            if sender
                .send(Message::Text(log_line.to_json()))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    // Subscribe to new logs
    let mut log_rx = app.log_tx.subscribe();

    // Spawn a task to send new logs
    let send_task = tokio::spawn(async move {
        while let Ok(log_line) = log_rx.recv().await {
            if sender
                .send(Message::Text(log_line.to_json()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Wait for client to disconnect or send close
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Close(_)) => break,
            Err(_) => break,
            _ => {} // Ignore other messages (ping/pong handled automatically)
        }
    }

    send_task.abort();
}

/// HTTP endpoint to get log history as JSON
async fn logs_history(State(app): State<Arc<AppState>>) -> Json<Vec<serde_json::Value>> {
    let buffer = app.log_buffer.lock().await;
    let logs: Vec<serde_json::Value> = buffer
        .get_all()
        .iter()
        .map(|line| {
            serde_json::json!({
                "timestamp": line.timestamp,
                "stream": line.stream,
                "content": line.content
            })
        })
        .collect();
    Json(logs)
}

/// System statistics endpoint
async fn system_stats(State(app): State<Arc<AppState>>) -> Json<SystemStats> {
    // Refresh system info
    {
        let mut sys = app.sys_info.write().await;
        sys.refresh_cpu_all();
        sys.refresh_memory();
    }

    // Small delay to get accurate CPU readings after refresh
    tokio::time::sleep(Duration::from_millis(200)).await;

    {
        let mut sys = app.sys_info.write().await;
        sys.refresh_cpu_all();
    }

    let sys = app.sys_info.read().await;

    // CPU stats
    let cpus = sys.cpus();
    let cpu_usage: f32 = if cpus.is_empty() {
        0.0
    } else {
        cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32
    };
    let per_core_usage: Vec<f32> = cpus.iter().map(|cpu| cpu.cpu_usage()).collect();

    let cpu = CpuStats {
        usage_percent: cpu_usage,
        core_count: cpus.len(),
        per_core_usage,
    };

    // Memory stats
    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let free_memory = sys.free_memory();
    let available_memory = sys.available_memory();
    let memory_usage_percent = if total_memory > 0 {
        (used_memory as f32 / total_memory as f32) * 100.0
    } else {
        0.0
    };

    let memory = MemoryStats {
        total_bytes: total_memory,
        used_bytes: used_memory,
        free_bytes: free_memory,
        available_bytes: available_memory,
        usage_percent: memory_usage_percent,
        swap_total_bytes: sys.total_swap(),
        swap_used_bytes: sys.used_swap(),
    };

    // Disk stats
    let disks_info = Disks::new_with_refreshed_list();
    let disks: Vec<DiskStats> = disks_info
        .iter()
        .map(|disk| {
            let total = disk.total_space();
            let available = disk.available_space();
            let used = total.saturating_sub(available);
            let usage_percent = if total > 0 {
                (used as f32 / total as f32) * 100.0
            } else {
                0.0
            };
            DiskStats {
                name: disk.name().to_string_lossy().to_string(),
                mount_point: disk.mount_point().to_string_lossy().to_string(),
                total_bytes: total,
                available_bytes: available,
                used_bytes: used,
                usage_percent,
            }
        })
        .collect();

    // Load average (Unix only)
    let load_avg = System::load_average();
    let load_average = LoadAverage {
        one: load_avg.one,
        five: load_avg.five,
        fifteen: load_avg.fifteen,
    };

    Json(SystemStats {
        cpu,
        memory,
        disks,
        uptime_seconds: System::uptime(),
        load_average,
    })
}

/// Health check endpoint that checks the status of all services
async fn health_check(State(_app): State<Arc<AppState>>) -> Result<Json<HealthResponse>, ApiError> {
    // Define all services to check
    let services = vec![
        ServiceDefinition {
            name: "rollup",
            env_var: "ROLLUP_RPC_URL",
            default_url: "http://127.0.0.1:12346",
            health_path: "/healthcheck",
            optional_env: None,
        },
        ServiceDefinition {
            name: "worker",
            env_var: "BIND_ADDR",
            default_url: "http://127.0.0.1:8080",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            name: "fvk-service",
            env_var: "MIDNIGHT_FVK_SERVICE_URL",
            default_url: "http://127.0.0.1:8088",
            health_path: "/health",
            optional_env: Some("POOL_FVK_PK"),
        },
        ServiceDefinition {
            name: "indexer",
            env_var: "INDEXER_BIND",
            default_url: "http://127.0.0.1:13100",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            name: "mcp",
            env_var: "MCP_SERVER_BIND_ADDRESS",
            default_url: "http://127.0.0.1:3000",
            health_path: "/health", // MCP may not have health, we'll do TCP check
            optional_env: None,
        },
        ServiceDefinition {
            name: "ligero-proving-system",
            env_var: "PROVER_BIND_ADDR",
            default_url: "http://127.0.0.1:1313",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            name: "metrics",
            env_var: "METRICS_API_BIND",
            default_url: "http://127.0.0.1:13200",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            name: "oracle",
            env_var: "ORACLE_SERVER_BIND_ADDRESS",
            default_url: "http://127.0.0.1:8090",
            health_path: "/", // Oracle uses root endpoint for health check
            optional_env: None,
        },
        ServiceDefinition {
            name: "midnight-proof-pool-service",
            env_var: "PROOF_POOL_BIND_ADDR",
            default_url: "http://127.0.0.1:11235",
            health_path: "/health",
            optional_env: None,
        },
    ];

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create HTTP client: {}", e),
            )
        })?;

    let mut service_results = Vec::new();
    let mut all_healthy = true;

    for svc in services {
        // Check if optional service should be skipped
        if let Some(required_env) = svc.optional_env {
            if std::env::var(required_env).is_err() {
                // Skip this optional service
                continue;
            }
        }

        let base_url = resolve_service_url(svc.env_var, svc.default_url);
        let health_url = format!("{}{}", base_url, svc.health_path);

        let start = std::time::Instant::now();
        let result = check_service_health(&client, &health_url).await;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        let (status, error) = match result {
            Ok(_) => ("healthy".to_string(), None),
            Err(e) => {
                all_healthy = false;
                ("unhealthy".to_string(), Some(e))
            }
        };

        service_results.push(ServiceHealth {
            name: svc.name.to_string(),
            url: base_url,
            status,
            error,
            response_time_ms: Some(elapsed_ms),
        });
    }

    let overall_status = if all_healthy { "healthy" } else { "unhealthy" };

    Ok(Json(HealthResponse {
        status: overall_status.to_string(),
        services: service_results,
        checked_at: chrono::Utc::now().to_rfc3339(),
    }))
}

/// Resolve the service URL from environment variable or use default
fn resolve_service_url(env_var: &str, default_url: &str) -> String {
    if let Ok(value) = std::env::var(env_var) {
        let value = value.trim();
        if !value.is_empty() {
            // If it looks like a URL, use it directly
            if value.starts_with("http://") || value.starts_with("https://") {
                return value.to_string();
            }
            // Otherwise, assume it's a host:port and prepend http://
            return format!("http://{}", value);
        }
    }
    default_url.to_string()
}

/// Check if a service is healthy by calling its health endpoint
async fn check_service_health(client: &reqwest::Client, url: &str) -> Result<(), String> {
    match client.get(url).send().await {
        Ok(response) => {
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("HTTP {}", response.status()))
            }
        }
        Err(e) => {
            if e.is_connect() {
                Err("Connection refused".to_string())
            } else if e.is_timeout() {
                Err("Timeout".to_string())
            } else {
                Err(format!("{}", e))
            }
        }
    }
}

async fn stop_services(app: &Arc<AppState>) -> ApiResult {
    let mut state = app.state.lock().await;
    if !state.is_running() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Services already stopped",
        ));
    }

    let pid = state.child.as_ref().and_then(|child| child.id());
    let pgid = state.process_group;

    if let Some(pgid) = pgid {
        send_signal_to_group(pgid, "-TERM").await?;
    } else if let Some(pid) = pid {
        send_signal(pid, "-TERM").await?;
    }

    let stopped = if let Some(child) = state.child.as_mut() {
        match timeout(Duration::from_secs(10), child.wait()).await {
            Ok(Ok(_)) => true,
            Ok(Err(err)) => {
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to stop services: {err}"),
                ))
            }
            Err(_) => false,
        }
    } else if let Some(pgid) = pgid {
        wait_for_group_exit(pgid, Duration::from_secs(10)).await
    } else {
        true
    };

    if stopped {
        state.child = None;
        state.process_group = None;
        return Ok("Services stopped".to_string());
    }

    if let Some(pgid) = pgid {
        send_signal_to_group(pgid, "-KILL").await?;
    } else if let Some(pid) = pid {
        send_signal(pid, "-KILL").await?;
    }

    if let Some(child) = state.child.as_mut() {
        let _ = child.wait().await;
    } else if let Some(pgid) = pgid {
        let _ = wait_for_group_exit(pgid, Duration::from_secs(5)).await;
    }

    state.child = None;
    state.process_group = None;

    Ok("Services stopped (forced)".to_string())
}

async fn send_signal(pid: u32, signal: &str) -> Result<(), ApiError> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send {signal} to {pid}: {err}"),
            )
        })?;

    if !status.success() {
        eprintln!("kill {signal} {pid} exited with status {status}");
    }

    Ok(())
}

async fn send_signal_to_group(pgid: i32, signal: &str) -> Result<(), ApiError> {
    let status = Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pgid}"))
        .status()
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send {signal} to group {pgid}: {err}"),
            )
        })?;

    if !status.success() {
        eprintln!("kill {signal} -- -{pgid} exited with status {status}");
    }

    Ok(())
}

#[cfg(unix)]
fn process_group_running(pgid: i32) -> bool {
    let status = std::process::Command::new("kill")
        .arg("-0")
        .arg("--")
        .arg(format!("-{pgid}"))
        .status();
    match status {
        Ok(status) => status.success(),
        Err(err) => {
            eprintln!("Failed to check process group {pgid}: {err}");
            false
        }
    }
}

#[cfg(not(unix))]
fn process_group_running(_pgid: i32) -> bool {
    false
}

async fn wait_for_group_exit(pgid: i32, timeout_duration: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    while tokio::time::Instant::now() < deadline {
        if !process_group_running(pgid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    !process_group_running(pgid)
}
