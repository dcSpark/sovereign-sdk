use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sysinfo::{Disks, System};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::signal;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::time::{timeout, Duration};

/// Maximum number of log lines to keep in the buffer for new connections
const LOG_BUFFER_SIZE: usize = 1000;

/// Broadcast channel capacity
const BROADCAST_CAPACITY: usize = 256;

/// Databases cleaned by `/clean-database`
const CLEAN_DATABASES: [&str; 4] = ["da", "indexer", "fvk", "mcp_sessions"];

#[derive(Clone, Copy)]
enum StartMode {
    Always,
    WhenEnvFlag(&'static str),
    WhenEnvPresent(&'static str),
}

#[derive(Clone, Copy)]
struct ManagedServiceDefinition {
    id: &'static str,
    display_name: &'static str,
    script: &'static str,
    start_mode: StartMode,
}

const MANAGED_SERVICES: [ManagedServiceDefinition; 7] = [
    ManagedServiceDefinition {
        id: "oracle",
        display_name: "oracle",
        script: "run_oracle.sh",
        start_mode: StartMode::WhenEnvFlag("START_ORACLE"),
    },
    ManagedServiceDefinition {
        id: "rollup",
        display_name: "rollup",
        script: "run_rollup.sh",
        start_mode: StartMode::Always,
    },
    ManagedServiceDefinition {
        id: "worker",
        display_name: "worker",
        script: "run_verifier_service.sh",
        start_mode: StartMode::Always,
    },
    ManagedServiceDefinition {
        id: "fvk",
        display_name: "fvk",
        script: "run_fvk_service.sh",
        start_mode: StartMode::WhenEnvPresent("POOL_FVK_PK"),
    },
    ManagedServiceDefinition {
        id: "indexer",
        display_name: "indexer",
        script: "run_indexer.sh",
        start_mode: StartMode::Always,
    },
    ManagedServiceDefinition {
        id: "mcp",
        display_name: "mcp",
        script: "run_mcp.sh",
        start_mode: StartMode::Always,
    },
    ManagedServiceDefinition {
        id: "metrics",
        display_name: "metrics",
        script: "run_metrics.sh",
        start_mode: StartMode::Always,
    },
];

fn env_flag(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_present(name: &str) -> bool {
    std::env::var(name)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn service_config_names(service_id: &str) -> Vec<String> {
    let mut names = vec![service_id.to_string()];
    match service_id {
        "worker" => names.push("verifier".to_string()),
        "fvk" => {
            names.push("fvk-service".to_string());
            names.push("fvk_service".to_string());
        }
        _ => {}
    }
    names
}

fn service_config_env_keys(service_id: &str, suffix: &str) -> Vec<String> {
    service_config_names(service_id)
        .into_iter()
        .map(|name| {
            format!(
                "SERVICE_{}_{}",
                name.replace('-', "_").to_ascii_uppercase(),
                suffix
            )
        })
        .collect()
}

fn primary_service_remote_env(service_id: &str) -> String {
    service_config_env_keys(service_id, "REMOTE")
        .into_iter()
        .next()
        .unwrap_or_else(|| "SERVICE_UNKNOWN_REMOTE".to_string())
}

fn service_is_remote(service_id: &str) -> bool {
    service_config_env_keys(service_id, "REMOTE")
        .into_iter()
        .any(|env_key| env_flag(&env_key))
}

fn resolve_env_url(env_var: &str) -> Option<String> {
    let value = std::env::var(env_var).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value.starts_with("http://") || value.starts_with("https://") {
        Some(value.to_string())
    } else {
        Some(format!("http://{}", value))
    }
}

fn should_start_by_default(service: &ManagedServiceDefinition) -> bool {
    match service.start_mode {
        StartMode::Always => true,
        StartMode::WhenEnvFlag(env_name) => env_flag(env_name),
        StartMode::WhenEnvPresent(env_name) => env_present(env_name),
    }
}

fn service_order(service_id: &str) -> usize {
    MANAGED_SERVICES
        .iter()
        .position(|service| service.id == service_id)
        .unwrap_or(usize::MAX)
}

fn find_managed_service(service: &str) -> Option<&'static ManagedServiceDefinition> {
    let normalized = service.trim().to_ascii_lowercase();
    let canonical = match normalized.as_str() {
        "verifier" | "proof-verifier" => "worker",
        other => other,
    };

    MANAGED_SERVICES.iter().find(|entry| entry.id == canonical)
}

fn resolve_managed_service(service: &str) -> Result<&'static ManagedServiceDefinition, ApiError> {
    find_managed_service(service).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "Unknown service '{service}'. Supported services: {}",
                supported_service_ids()
            ),
        )
    })
}

fn supported_service_ids() -> String {
    MANAGED_SERVICES
        .iter()
        .map(|service| service.id)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub timestamp: String,
    pub service: String,
    pub stream: String, // "stdout" or "stderr"
    pub content: String,
}

impl LogLine {
    fn to_json(&self) -> String {
        serde_json::json!({
            "timestamp": self.timestamp,
            "service": self.service,
            "stream": self.stream,
            "content": self.content
        })
        .to_string()
    }
}

struct ManagedProcess {
    child: tokio::process::Child,
    process_group: Option<i32>,
}

#[derive(Default)]
struct ServiceState {
    services: HashMap<String, ManagedProcess>,
}

impl ServiceState {
    fn refresh_service(&mut self, service_id: &str) -> bool {
        let mut remove = false;
        let mut running = false;

        if let Some(process) = self.services.get_mut(service_id) {
            match process.child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => running = true,
                Err(err) => {
                    eprintln!("Failed to check service status for {service_id}: {err}");
                }
            }

            if !running {
                if let Some(pgid) = process.process_group {
                    if process_group_running(pgid) {
                        running = true;
                    } else {
                        process.process_group = None;
                    }
                }
            }

            remove = !running;
        }

        if remove {
            self.services.remove(service_id);
        }

        running
    }

    fn refresh_all(&mut self) {
        let service_ids = self.services.keys().cloned().collect::<Vec<_>>();
        for service_id in service_ids {
            self.refresh_service(&service_id);
        }
    }

    fn any_running(&mut self) -> bool {
        self.refresh_all();
        !self.services.is_empty()
    }

    fn snapshot_running(&mut self) -> HashMap<String, Option<u32>> {
        self.refresh_all();

        let mut snapshot = HashMap::new();
        let service_ids = self.services.keys().cloned().collect::<Vec<_>>();
        for service_id in service_ids {
            let pid = self
                .services
                .get_mut(&service_id)
                .and_then(|process| process.child.id());
            snapshot.insert(service_id, pid);
        }

        snapshot
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
    script_dir: PathBuf,
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

#[derive(Debug, Serialize)]
pub struct ManagedServiceStatus {
    pub id: String,
    pub name: String,
    pub script: String,
    pub start_by_default: bool,
    pub remote: bool,
    pub controllable: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
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
    pub id: String,
    pub name: String,
    pub url: String,
    pub status: String,
    pub remote: bool,
    pub controllable: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
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
    id: &'static str,
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
    let demo_data_dir = script_dir.join("demo_data");

    let bind_addr =
        std::env::var("SERVICE_CONTROLLER_BIND").unwrap_or_else(|_| "127.0.0.1:9090".to_string());

    for service in MANAGED_SERVICES {
        let script_path = script_dir.join(service.script);
        if !script_path.exists() {
            anyhow::bail!(
                "Missing script for service '{}' at {}",
                service.id,
                script_path.display()
            );
        }
    }

    let (log_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

    // Initialize system info
    let mut sys = System::new_all();
    sys.refresh_all();

    let app_state = Arc::new(AppState {
        script_dir,
        demo_data_dir,
        state: Mutex::new(ServiceState::default()),
        log_tx,
        log_buffer: Arc::new(Mutex::new(LogBuffer::new())),
        sys_info: RwLock::new(sys),
    });

    let app = Router::new()
        .route("/start", post(start).get(start))
        .route("/start/:service", post(start_service).get(start_service))
        .route("/stop", post(stop).get(stop))
        .route("/stop/:service", post(stop_service).get(stop_service))
        .route("/restart", post(restart).get(restart))
        .route(
            "/restart/:service",
            post(restart_service).get(restart_service),
        )
        .route("/clean", post(clean).get(clean))
        .route("/clean-database", post(clean_database).get(clean_database))
        .route("/reset-tee", post(reset_tee).get(reset_tee))
        .route("/services", get(services))
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
    let mut running_services = {
        let mut state = app.state.lock().await;
        state.refresh_all();
        state.services.keys().cloned().collect::<Vec<_>>()
    };

    if running_services.is_empty() {
        println!("No services running, nothing to stop.");
        return Ok(());
    }

    running_services.sort_by_key(|service_id| Reverse(service_order(service_id)));

    println!("Stopping child services...");

    for service_id in running_services {
        if let Some(service) = find_managed_service(&service_id) {
            match stop_single_service(app, service).await {
                Ok(message) => println!("{message}"),
                Err(err) if err.status == StatusCode::CONFLICT => {}
                Err(err) => {
                    eprintln!(
                        "Warning: failed to stop service '{}' during shutdown: {}",
                        service.id, err.message
                    );
                }
            }
        }
    }

    Ok(())
}

async fn maybe_clear_logs_on_fresh_start(app: &Arc<AppState>) {
    let should_clear = {
        let mut state = app.state.lock().await;
        !state.any_running()
    };

    if should_clear {
        let mut buffer = app.log_buffer.lock().await;
        buffer.clear();
    }
}

fn spawn_log_reader<R>(
    reader: R,
    stream: &'static str,
    service_id: &'static str,
    log_tx: broadcast::Sender<LogLine>,
    log_buffer: Arc<Mutex<LogBuffer>>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let service = service_id.to_string();
    tokio::spawn(async move {
        let reader = BufReader::new(reader);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let log_line = LogLine {
                timestamp: chrono::Utc::now().to_rfc3339(),
                service: service.clone(),
                stream: stream.to_string(),
                content: line.clone(),
            };

            if stream == "stdout" {
                println!("[{service}] {line}");
            } else {
                eprintln!("[{service}] {line}");
            }

            {
                let mut buffer = log_buffer.lock().await;
                buffer.push(log_line.clone());
            }

            let _ = log_tx.send(log_line);
        }
    });
}

async fn start_single_service(
    app: &Arc<AppState>,
    service: &'static ManagedServiceDefinition,
) -> ApiResult {
    if service_is_remote(service.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "Service '{}' is configured as remote and cannot be started locally (set {}=0 to re-enable local actions)",
                service.id,
                primary_service_remote_env(service.id)
            ),
        ));
    }

    let script_path = app.script_dir.join(service.script);
    if !script_path.exists() {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Missing script for service '{}' at {}",
                service.id,
                script_path.display()
            ),
        ));
    }

    let mut state = app.state.lock().await;
    if state.refresh_service(service.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("Service '{}' already running", service.id),
        ));
    }

    let mut command = Command::new("bash");
    command
        .arg(script_path)
        .current_dir(&app.script_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|err| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to start service '{}': {err}", service.id),
        )
    })?;

    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    state.services.insert(
        service.id.to_string(),
        ManagedProcess {
            child,
            #[cfg(unix)]
            process_group: pid.map(|pid| pid as i32),
            #[cfg(not(unix))]
            process_group: None,
        },
    );

    drop(state);

    if let Some(stdout) = stdout {
        spawn_log_reader(
            stdout,
            "stdout",
            service.id,
            app.log_tx.clone(),
            Arc::clone(&app.log_buffer),
        );
    }

    if let Some(stderr) = stderr {
        spawn_log_reader(
            stderr,
            "stderr",
            service.id,
            app.log_tx.clone(),
            Arc::clone(&app.log_buffer),
        );
    }

    let message = match pid {
        Some(pid) => format!("Service '{}' starting (pid {pid})", service.id),
        None => format!("Service '{}' starting", service.id),
    };

    Ok(message)
}

async fn stop_single_service(
    app: &Arc<AppState>,
    service: &'static ManagedServiceDefinition,
) -> ApiResult {
    if service_is_remote(service.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "Service '{}' is configured as remote and cannot be stopped locally",
                service.id
            ),
        ));
    }

    let mut state = app.state.lock().await;
    if !state.refresh_service(service.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("Service '{}' already stopped", service.id),
        ));
    }

    let (pid, pgid) = match state.services.get_mut(service.id) {
        Some(process) => (process.child.id(), process.process_group),
        None => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("Service '{}' already stopped", service.id),
            ))
        }
    };

    if let Some(pgid) = pgid {
        send_signal_to_group(pgid, "-TERM").await?;
    } else if let Some(pid) = pid {
        send_signal(pid, "-TERM").await?;
    }

    let stopped = if let Some(process) = state.services.get_mut(service.id) {
        match timeout(Duration::from_secs(10), process.child.wait()).await {
            Ok(Ok(_)) => true,
            Ok(Err(err)) => {
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to stop service '{}': {err}", service.id),
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
        state.services.remove(service.id);
        return Ok(format!("Service '{}' stopped", service.id));
    }

    if let Some(pgid) = pgid {
        send_signal_to_group(pgid, "-KILL").await?;
    } else if let Some(pid) = pid {
        send_signal(pid, "-KILL").await?;
    }

    if let Some(process) = state.services.get_mut(service.id) {
        let _ = process.child.wait().await;
    } else if let Some(pgid) = pgid {
        let _ = wait_for_group_exit(pgid, Duration::from_secs(5)).await;
    }

    state.services.remove(service.id);

    Ok(format!("Service '{}' stopped (forced)", service.id))
}

async fn start(State(app): State<Arc<AppState>>) -> ApiResult {
    maybe_clear_logs_on_fresh_start(&app).await;

    let skipped_remote = MANAGED_SERVICES
        .iter()
        .filter(|service| should_start_by_default(service) && service_is_remote(service.id))
        .map(|service| service.id)
        .collect::<Vec<_>>();

    let services_to_start = MANAGED_SERVICES
        .iter()
        .filter(|service| should_start_by_default(service) && !service_is_remote(service.id))
        .collect::<Vec<_>>();

    if services_to_start.is_empty() {
        if skipped_remote.is_empty() {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "No services configured for default start",
            ));
        }

        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "All default services are configured as remote: {}",
                skipped_remote.join(", ")
            ),
        ));
    }

    let mut started = Vec::new();
    let mut already_running = Vec::new();

    for service in services_to_start {
        match start_single_service(&app, service).await {
            Ok(_) => started.push(service.id),
            Err(err) if err.status == StatusCode::CONFLICT => already_running.push(service.id),
            Err(err) => {
                return Err(ApiError::new(
                    err.status,
                    format!("Failed to start '{}': {}", service.id, err.message),
                ))
            }
        }
    }

    if started.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "Default services already running: {}",
                already_running.join(", ")
            ),
        ));
    }

    let mut message = if already_running.is_empty() {
        format!("Started services: {}", started.join(", "))
    } else {
        format!(
            "Started services: {} (already running: {})",
            started.join(", "),
            already_running.join(", ")
        )
    };

    if !skipped_remote.is_empty() {
        message.push_str(&format!(
            " (remote and not started locally: {})",
            skipped_remote.join(", ")
        ));
    }

    Ok(message)
}

async fn start_service(Path(service): Path<String>, State(app): State<Arc<AppState>>) -> ApiResult {
    let service = resolve_managed_service(&service)?;
    maybe_clear_logs_on_fresh_start(&app).await;
    start_single_service(&app, service).await
}

async fn stop(State(app): State<Arc<AppState>>) -> ApiResult {
    let mut running_services = {
        let mut state = app.state.lock().await;
        state.refresh_all();
        state.services.keys().cloned().collect::<Vec<_>>()
    };

    if running_services.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Services already stopped",
        ));
    }

    running_services.sort_by_key(|service_id| Reverse(service_order(service_id)));

    let mut stopped = Vec::new();
    let mut failures = Vec::new();

    for service_id in running_services {
        if let Some(service) = find_managed_service(&service_id) {
            match stop_single_service(&app, service).await {
                Ok(_) => stopped.push(service.id),
                Err(err) => failures.push(format!("{}: {}", service.id, err.message)),
            }
        }
    }

    if failures.is_empty() {
        Ok(format!("Stopped services: {}", stopped.join(", ")))
    } else {
        Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Stopped services: {}. Failures: {}",
                if stopped.is_empty() {
                    "none".to_string()
                } else {
                    stopped.join(", ")
                },
                failures.join(" | ")
            ),
        ))
    }
}

async fn stop_service(Path(service): Path<String>, State(app): State<Arc<AppState>>) -> ApiResult {
    let service = resolve_managed_service(&service)?;
    stop_single_service(&app, service).await
}

async fn restart(State(app): State<Arc<AppState>>) -> ApiResult {
    match stop(State(app.clone())).await {
        Ok(_) => {}
        Err(err) if err.status == StatusCode::CONFLICT => {}
        Err(err) => return Err(err),
    }

    start(State(app)).await
}

async fn restart_service(
    Path(service): Path<String>,
    State(app): State<Arc<AppState>>,
) -> ApiResult {
    let service = resolve_managed_service(&service)?;

    match stop_single_service(&app, service).await {
        Ok(_) => {}
        Err(err) if err.status == StatusCode::CONFLICT => {}
        Err(err) => return Err(err),
    }

    start_single_service(&app, service).await
}

async fn clean(State(app): State<Arc<AppState>>) -> ApiResult {
    ensure_services_stopped_for_clean_like_actions(&app).await?;

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

async fn ensure_services_stopped_for_clean_like_actions(app: &Arc<AppState>) -> Result<(), ApiError> {
    let mut state = app.state.lock().await;
    if state.any_running() {
        return Err(ApiError::new(StatusCode::CONFLICT, "Services must be stopped"));
    }
    Ok(())
}

fn escape_pg_identifier(value: &str) -> String {
    value.replace('"', "\"\"")
}

async fn clean_database(State(app): State<Arc<AppState>>) -> ApiResult {
    ensure_services_stopped_for_clean_like_actions(&app).await?;

    let connection_string = std::env::var("DA_CONNECTION_STRING").map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DA_CONNECTION_STRING is not set",
        )
    })?;

    if !(connection_string.starts_with("postgres://")
        || connection_string.starts_with("postgresql://"))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "DA_CONNECTION_STRING must use postgres/postgresql scheme for /clean-database",
        ));
    }

    let base_options = PgConnectOptions::from_str(&connection_string).map_err(|err| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid DA_CONNECTION_STRING: {err}"),
        )
    })?;

    let mut summary = Vec::new();

    for database_name in CLEAN_DATABASES {
        let options = base_options.clone().database(database_name);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .map_err(|err| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to connect to database '{database_name}': {err}"),
                )
            })?;

        let mut tx = pool.begin().await.map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to open transaction for '{database_name}': {err}"),
            )
        })?;

        let tables: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT schemaname, tablename
            FROM pg_catalog.pg_tables
            WHERE schemaname NOT IN ('pg_catalog', 'information_schema')
            ORDER BY schemaname, tablename
            "#,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list tables for '{database_name}': {err}"),
            )
        })?;

        for (schema_name, table_name) in &tables {
            let drop_stmt = format!(
                "DROP TABLE IF EXISTS \"{}\".\"{}\" CASCADE",
                escape_pg_identifier(schema_name),
                escape_pg_identifier(table_name)
            );

            sqlx::query(&drop_stmt).execute(&mut *tx).await.map_err(|err| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "Failed to drop table '{}.{}' in '{}': {err}",
                        schema_name, table_name, database_name
                    ),
                )
            })?;
        }

        tx.commit().await.map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to commit cleanup for '{database_name}': {err}"),
            )
        })?;

        summary.push(format!("{database_name}: {}", tables.len()));
    }

    Ok(format!(
        "Cleaned databases (tables dropped): {}",
        summary.join(", ")
    ))
}

async fn reset_tee(State(app): State<Arc<AppState>>) -> ApiResult {
    ensure_services_stopped_for_clean_like_actions(&app).await?;

    let reset_url = std::env::var("TEE_RESET_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "http://74.235.106.62:9898/reset".to_string());

    let token = std::env::var("TEE_RESET_BEARER_TOKEN")
        .or_else(|_| std::env::var("TEE_RESET_TOKEN"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TEE reset token not configured (set TEE_RESET_BEARER_TOKEN)",
            )
        })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create HTTP client for TEE reset: {err}"),
            )
        })?;

    let response = client
        .post(&reset_url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("TEE reset request failed: {err}"),
            )
        })?;

    if response.status() == StatusCode::NO_CONTENT {
        return Ok("TEE reset successful".to_string());
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = if body.trim().is_empty() {
        format!("TEE reset failed: upstream returned {}", status)
    } else {
        format!(
            "TEE reset failed: upstream returned {} ({})",
            status,
            body.trim()
        )
    };

    Err(ApiError::new(StatusCode::BAD_GATEWAY, message))
}

async fn services(State(app): State<Arc<AppState>>) -> Json<Vec<ManagedServiceStatus>> {
    let running_snapshot = {
        let mut state = app.state.lock().await;
        state.snapshot_running()
    };

    let services = MANAGED_SERVICES
        .iter()
        .map(|service| {
            let remote = service_is_remote(service.id);
            ManagedServiceStatus {
                id: service.id.to_string(),
                name: service.display_name.to_string(),
                script: service.script.to_string(),
                start_by_default: should_start_by_default(service) && !remote,
                remote,
                controllable: !remote,
                running: !remote && running_snapshot.contains_key(service.id),
                pid: if remote {
                    None
                } else {
                    running_snapshot.get(service.id).copied().flatten()
                },
            }
        })
        .collect::<Vec<_>>();

    Json(services)
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
                "service": line.service,
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
async fn health_check(State(app): State<Arc<AppState>>) -> Result<Json<HealthResponse>, ApiError> {
    let running_snapshot = {
        let mut state = app.state.lock().await;
        state.snapshot_running()
    };

    // Define all services to check
    let services = vec![
        ServiceDefinition {
            id: "rollup",
            name: "rollup",
            env_var: "ROLLUP_RPC_URL",
            default_url: "http://127.0.0.1:12346",
            health_path: "/healthcheck",
            optional_env: None,
        },
        ServiceDefinition {
            id: "worker",
            name: "worker",
            env_var: "BIND_ADDR",
            default_url: "http://127.0.0.1:8080",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            id: "fvk",
            name: "fvk",
            env_var: "MIDNIGHT_FVK_SERVICE_URL",
            default_url: "http://127.0.0.1:8088",
            health_path: "/health",
            optional_env: Some("POOL_FVK_PK"),
        },
        ServiceDefinition {
            id: "indexer",
            name: "indexer",
            env_var: "INDEXER_BIND",
            default_url: "http://127.0.0.1:13100",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            id: "mcp",
            name: "mcp",
            env_var: "MCP_SERVER_BIND_ADDRESS",
            default_url: "http://127.0.0.1:3000",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            id: "metrics",
            name: "metrics",
            env_var: "METRICS_API_BIND",
            default_url: "http://127.0.0.1:13200",
            health_path: "/health",
            optional_env: None,
        },
        ServiceDefinition {
            id: "oracle",
            name: "oracle",
            env_var: "ORACLE_SERVER_BIND_ADDRESS",
            default_url: "http://127.0.0.1:8090",
            health_path: "/",
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
        let remote = service_is_remote(svc.id);
        let running = !remote && running_snapshot.contains_key(svc.id);
        let pid = if remote {
            None
        } else {
            running_snapshot.get(svc.id).copied().flatten()
        };

        let enabled_by_env = if let Some(required_env) = svc.optional_env {
            env_present(required_env)
        } else {
            true
        };

        // Hide optional services unless explicitly enabled or actively running.
        if !enabled_by_env && !running && !remote {
            continue;
        }

        let base_url = resolve_service_url(svc.id, svc.env_var, svc.default_url);
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
            id: svc.id.to_string(),
            name: svc.name.to_string(),
            url: base_url,
            status,
            remote,
            controllable: !remote,
            running,
            pid,
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
fn resolve_service_url(service_id: &str, env_var: &str, default_url: &str) -> String {
    for service_url_env in service_config_env_keys(service_id, "URL") {
        if let Some(url) = resolve_env_url(&service_url_env) {
            return url;
        }
    }

    if let Some(url) = resolve_env_url(env_var) {
        return url;
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
