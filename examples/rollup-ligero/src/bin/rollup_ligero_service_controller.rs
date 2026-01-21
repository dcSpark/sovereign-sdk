use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

#[derive(Default)]
struct ServiceState {
    child: Option<tokio::process::Child>,
    process_group: Option<i32>,
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

struct AppState {
    run_all_path: PathBuf,
    run_all_dir: PathBuf,
    demo_data_dir: PathBuf,
    state: Mutex<ServiceState>,
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

    let app_state = Arc::new(AppState {
        run_all_dir: script_dir,
        run_all_path,
        demo_data_dir,
        state: Mutex::new(ServiceState::default()),
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
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to bind to {bind_addr}"))?;
    println!("Service controller listening on http://{bind_addr}");
    axum::serve(listener, app).await?;
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

    let mut command = Command::new("bash");
    command
        .arg(&app.run_all_path)
        .current_dir(&app.run_all_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(|err| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to start services: {err}"),
        )
    })?;

    let pid = child.id();
    state.child = Some(child);
    #[cfg(unix)]
    {
        state.process_group = pid.map(|pid| pid as i32);
    }
    #[cfg(not(unix))]
    {
        state.process_group = None;
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

/// Health check endpoint that checks the status of all services
async fn health_check(
    State(_app): State<Arc<AppState>>,
) -> Result<Json<HealthResponse>, ApiError> {
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
            name: "verifier",
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
            name: "mcp-2",
            env_var: "MCP_SERVER_BIND_ADDRESS_2",
            default_url: "http://127.0.0.1:3001",
            health_path: "/health", // MCP may not have health, we'll do TCP check
            optional_env: None,
        },
        ServiceDefinition {
            name: "prover",
            env_var: "PROVER_BIND_ADDR",
            default_url: "http://127.0.0.1:1313",
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
