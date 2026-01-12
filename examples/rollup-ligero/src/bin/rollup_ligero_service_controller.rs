use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
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
                format!(
                    "Failed to remove {}: {err}",
                    app.demo_data_dir.display()
                ),
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
