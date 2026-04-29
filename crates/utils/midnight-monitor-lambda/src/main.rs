use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use lambda_runtime::{run, service_fn, Error, LambdaEvent};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::process::Command;
use tracing::{error, info, warn};

const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;
const DEFAULT_STRESS_WALLETS: usize = 3;
const DEFAULT_STRESS_TXS: usize = 1;
const DEFAULT_STRESS_SESSION_IDS_FILE: &str = "/tmp/mcp-external-stress-session-ids.txt";
const DEFAULT_STRESS_SCRIPT_PATH: &str = "scripts/mcp-external-stress.sh";
const DEFAULT_TEE_RESET_URL: &str = "http://74.235.106.62:9898/reset";
const DEFAULT_DISK_USAGE_MOUNT_PATH: &str = "/";
const MONITOR_METRICS_NAMESPACE: &str = "Midnight/Monitor";
const S5_PEAK_TPS_CHECK: &str = "metrics:s5-peak-tps";
const PROOF_POOL_SEND_CHECK: &str = "proof-pool:send";
const TEE_RESET_ENDPOINT_CHECK: &str = "tee:reset-endpoint";
const MCP_STRESS_CHECK: &str = "mcp:stress";
const DISK_USAGE_CHECK: &str = "system:disk-usage";

#[derive(Clone, Copy)]
struct ServiceCheckDef {
    id: &'static str,
    health_path: &'static str,
    check: &'static str,
}

const SERVICE_CHECKS: [ServiceCheckDef; 8] = [
    ServiceCheckDef {
        id: "rollup",
        health_path: "/healthcheck",
        check: "health:rollup",
    },
    ServiceCheckDef {
        id: "worker",
        health_path: "/health",
        check: "health:worker",
    },
    ServiceCheckDef {
        id: "fvk",
        health_path: "/health",
        check: "health:fvk",
    },
    ServiceCheckDef {
        id: "indexer",
        health_path: "/health",
        check: "health:indexer",
    },
    ServiceCheckDef {
        id: "mcp",
        health_path: "/health",
        check: "health:mcp",
    },
    ServiceCheckDef {
        id: "metrics",
        health_path: "/health",
        check: "health:metrics",
    },
    ServiceCheckDef {
        id: "oracle",
        health_path: "/",
        check: "health:oracle",
    },
    ServiceCheckDef {
        id: "proof-pool",
        health_path: "/health",
        check: "health:proof-pool",
    },
];

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    monitor_env: String,
    proof_pool_auth_token: Option<String>,
    controller_basic_auth_username: Option<String>,
    controller_basic_auth_password: Option<String>,
    tee_reset_url: String,
    disk_usage_mount_path: String,
    http_timeout_secs: u64,
    stress_wallets: usize,
    stress_txs: usize,
    stress_script_path: PathBuf,
    stress_session_ids_file: PathBuf,
}

impl Config {
    fn from_env() -> Result<Self> {
        let base_url = required_env("BASE_URL")?;
        let tee_reset_url = optional_env("MONITOR_TEE_RESET_URL")
            .or_else(|| optional_env("TEE_RESET_URL"))
            .unwrap_or_else(|| DEFAULT_TEE_RESET_URL.to_string());
        let controller_basic_auth_username = optional_env("MONITOR_CONTROLLER_BASIC_AUTH_USERNAME")
            .or_else(|| optional_env("CONTROLLER_BASIC_AUTH_USERNAME"));
        let controller_basic_auth_password = optional_env("MONITOR_CONTROLLER_BASIC_AUTH_PASSWORD")
            .or_else(|| optional_env("CONTROLLER_BASIC_AUTH_PASSWORD"));

        match (
            controller_basic_auth_username.as_ref(),
            controller_basic_auth_password.as_ref(),
        ) {
            (Some(_), Some(_)) | (None, None) => {}
            _ => {
                bail!(
                    "MONITOR_CONTROLLER_BASIC_AUTH_USERNAME and MONITOR_CONTROLLER_BASIC_AUTH_PASSWORD must either both be set or both be unset"
                );
            }
        }

        reqwest::Url::parse(&tee_reset_url).with_context(|| {
            format!("TEE reset URL must be a valid absolute URL: {tee_reset_url}")
        })?;
        let stress_script_path = env::var_os("MCP_STRESS_SCRIPT_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(default_stress_script_path);

        Ok(Self {
            base_url: normalize_base_url(&base_url),
            monitor_env: required_env("MONITOR_ENV")?,
            proof_pool_auth_token: optional_env("PROOF_POOL_AUTH_TOKEN"),
            controller_basic_auth_username,
            controller_basic_auth_password,
            tee_reset_url,
            disk_usage_mount_path: optional_env("MONITOR_DISK_USAGE_MOUNT_PATH")
                .or_else(|| optional_env("DISK_USAGE_MOUNT_PATH"))
                .unwrap_or_else(|| DEFAULT_DISK_USAGE_MOUNT_PATH.to_string()),
            http_timeout_secs: env_parse("HTTP_TIMEOUT_SECS", DEFAULT_HTTP_TIMEOUT_SECS)?,
            stress_wallets: env_parse("STRESS_WALLETS", DEFAULT_STRESS_WALLETS)?,
            stress_txs: env_parse("STRESS_TXS", DEFAULT_STRESS_TXS)?,
            stress_script_path,
            stress_session_ids_file: PathBuf::from(
                env::var("MCP_STRESS_SESSION_IDS_FILE")
                    .unwrap_or_else(|_| DEFAULT_STRESS_SESSION_IDS_FILE.to_string()),
            ),
        })
    }
}

#[derive(Debug, Serialize, Clone)]
struct MonitorReport {
    request_id: String,
    environment: String,
    base_url: String,
    duration_ms: u128,
    all_checks_passed: bool,
    health_checks: Vec<HealthCheckResult>,
    s5_peak_tps: OperationCheckResult<S5PeakTpsResult>,
    proof_pool_send: OperationCheckResult<ProofPoolSendResult>,
    tee_reset_endpoint: OperationCheckResult<TeeEndpointResult>,
    mcp_stress: OperationCheckResult<StressRunResult>,
    disk_usage: OperationCheckResult<DiskUsageResult>,
}

#[derive(Debug, Serialize, Clone)]
struct HealthCheckResult {
    check: String,
    service: String,
    url: String,
    healthy: bool,
    status_code: u16,
    latency_ms: u128,
    body_excerpt: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ProofPoolSendResult {
    requested: usize,
    flushed: usize,
    accepted: usize,
    rejected: usize,
    ready_proofs: usize,
}

#[derive(Debug, Serialize, Clone)]
struct S5PeakTpsResult {
    url: String,
    status_code: u16,
    tps: f64,
    peak_tps: f64,
    peak_tps_at_ms: Option<i64>,
}

#[derive(Debug, Serialize, Clone)]
struct OperationCheckResult<T> {
    check: String,
    healthy: bool,
    latency_ms: u128,
    result: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct TeeEndpointResult {
    url: String,
    status_code: Option<u16>,
}

#[derive(Debug, Serialize, Clone)]
struct DiskUsageResult {
    url: String,
    disk_name: String,
    mount_path: String,
    usage_percent: f64,
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct ControllerSystemStatsResponse {
    disks: Vec<ControllerDiskStats>,
}

#[derive(Debug, Deserialize)]
struct ControllerDiskStats {
    name: String,
    mount_point: String,
    total_bytes: u64,
    available_bytes: u64,
    used_bytes: u64,
    usage_percent: f64,
}

#[derive(Debug, Serialize, Clone)]
struct StressRunResult {
    wallets: usize,
    txs_per_wallet: usize,
    endpoint: String,
    ready: u64,
    sends_ok: u64,
    sends_err: u64,
    sends_retried: Option<u64>,
    stdout_tail: String,
    stderr_tail: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .init();

    run(service_fn(handler)).await
}

async fn handler(event: LambdaEvent<Value>) -> Result<MonitorReport, Error> {
    let request_id = event.context.request_id;
    match run_monitor(request_id.clone()).await {
        Ok(report) => {
            info!(
                request_id = request_id,
                duration_ms = report.duration_ms,
                "monitor completed successfully"
            );
            Ok(report)
        }
        Err(err) => {
            error!(
                request_id = request_id,
                error = format!("{err:#}"),
                "monitor failed"
            );
            Err(err.into())
        }
    }
}

async fn run_monitor(request_id: String) -> Result<MonitorReport> {
    let started = Instant::now();
    let config = Config::from_env()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(config.http_timeout_secs))
        .user_agent("midnight-monitor-lambda")
        .build()
        .context("failed to build HTTP client")?;

    if config.proof_pool_auth_token.is_none() {
        warn!("PROOF_POOL_AUTH_TOKEN is not set; proof-pool send will fail if auth is required");
    }
    if config.controller_basic_auth_username.is_none() {
        warn!(
            "MONITOR_CONTROLLER_BASIC_AUTH_USERNAME is not set; disk usage metric collection will fail if /controller/stats requires basic auth"
        );
    }

    info!(base_url = config.base_url, "running service health checks");
    let health_checks = check_services(&client, &config.base_url).await;

    info!("running s5 PeakTPS check");
    let s5_peak_tps = run_s5_peak_tps_check(&client, &config).await;

    info!("running proof-pool send check");
    let proof_pool_send = run_proof_pool_send_check(&client, &config).await;

    info!("running tee reset endpoint check");
    let tee_reset_endpoint = run_tee_reset_endpoint_check(&client, &config).await;

    info!("running MCP stress check");
    let mcp_stress = run_mcp_stress_check(&config).await;

    info!(mount_path = %config.disk_usage_mount_path, "collecting disk usage metric");
    let disk_usage = run_disk_usage_check(&client, &config).await;
    if disk_usage.healthy {
        if let Some(result) = disk_usage.result.as_ref() {
            info!(
                mount_path = %result.mount_path,
                usage_percent = result.usage_percent,
                "collected disk usage metric"
            );
        }
    } else {
        warn!(
            mount_path = %config.disk_usage_mount_path,
            error = disk_usage.error.as_deref().unwrap_or("unknown error"),
            "disk usage metric collection failed"
        );
    }

    let all_checks_passed = health_checks.iter().all(|result| result.healthy)
        && s5_peak_tps.healthy
        && proof_pool_send.healthy
        && tee_reset_endpoint.healthy
        && mcp_stress.healthy;
    let report = MonitorReport {
        request_id,
        environment: config.monitor_env,
        base_url: config.base_url,
        duration_ms: started.elapsed().as_millis(),
        all_checks_passed,
        health_checks,
        s5_peak_tps,
        proof_pool_send,
        tee_reset_endpoint,
        mcp_stress,
        disk_usage,
    };

    emit_metrics(&report)?;

    if report.all_checks_passed {
        Ok(report)
    } else {
        bail!(
            "monitor checks failed for: {}",
            failed_checks(&report).join(", ")
        );
    }
}

async fn check_services(client: &Client, base_url: &str) -> Vec<HealthCheckResult> {
    let mut results = Vec::with_capacity(SERVICE_CHECKS.len());

    for service in SERVICE_CHECKS {
        results.push(check_service(client, base_url, service).await);
    }

    results
}

async fn check_service(
    client: &Client,
    base_url: &str,
    service: ServiceCheckDef,
) -> HealthCheckResult {
    let url = service_url(base_url, service.id, service.health_path);
    let started = Instant::now();

    match client.get(&url).send().await {
        Ok(response) => {
            let status = response.status();
            let body = match response.text().await {
                Ok(body) => body,
                Err(err) => {
                    return HealthCheckResult {
                        check: service.check.to_string(),
                        service: service.id.to_string(),
                        url,
                        healthy: false,
                        status_code: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        body_excerpt: None,
                        error: Some(format!("failed to read response body: {err}")),
                    };
                }
            };

            HealthCheckResult {
                check: service.check.to_string(),
                service: service.id.to_string(),
                url,
                healthy: status.is_success(),
                status_code: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                body_excerpt: excerpt(&body),
                error: None,
            }
        }
        Err(err) => HealthCheckResult {
            check: service.check.to_string(),
            service: service.id.to_string(),
            url,
            healthy: false,
            status_code: 0,
            latency_ms: started.elapsed().as_millis(),
            body_excerpt: None,
            error: Some(format!("{err:#}")),
        },
    }
}

async fn run_proof_pool_send_check(
    client: &Client,
    config: &Config,
) -> OperationCheckResult<ProofPoolSendResult> {
    let started = Instant::now();
    match execute_proof_pool_send_check(client, config).await {
        Ok(result) => OperationCheckResult {
            check: PROOF_POOL_SEND_CHECK.to_string(),
            healthy: true,
            latency_ms: started.elapsed().as_millis(),
            result: Some(result),
            error: None,
        },
        Err(err) => OperationCheckResult {
            check: PROOF_POOL_SEND_CHECK.to_string(),
            healthy: false,
            latency_ms: started.elapsed().as_millis(),
            result: None,
            error: Some(format!("{err:#}")),
        },
    }
}

#[derive(Debug, Deserialize)]
struct S5MetricsResponse {
    #[serde(rename = "TPS")]
    tps: f64,
    #[serde(rename = "PeakTPS")]
    peak_tps: f64,
    #[serde(rename = "PeakTPSAtMs")]
    peak_tps_at_ms: Option<i64>,
}

async fn run_s5_peak_tps_check(
    client: &Client,
    config: &Config,
) -> OperationCheckResult<S5PeakTpsResult> {
    let started = Instant::now();
    match execute_s5_peak_tps_check(client, config).await {
        Ok(result) => OperationCheckResult {
            check: S5_PEAK_TPS_CHECK.to_string(),
            healthy: true,
            latency_ms: started.elapsed().as_millis(),
            result: Some(result),
            error: None,
        },
        Err(err) => OperationCheckResult {
            check: S5_PEAK_TPS_CHECK.to_string(),
            healthy: false,
            latency_ms: started.elapsed().as_millis(),
            result: None,
            error: Some(format!("{err:#}")),
        },
    }
}

async fn execute_s5_peak_tps_check(client: &Client, config: &Config) -> Result<S5PeakTpsResult> {
    let url = format!("{}/metrics/s5", config.base_url);
    let response = client
        .get(&url)
        .send()
        .await
        .context("s5 metrics request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read s5 metrics response body")?;

    parse_s5_peak_tps_response(url, status.as_u16(), &body)
}

fn parse_s5_peak_tps_response(
    url: String,
    status_code: u16,
    body: &str,
) -> Result<S5PeakTpsResult> {
    if !(200..300).contains(&status_code) {
        bail!(
            "s5 metrics returned {}: {}",
            status_code,
            excerpt(body).unwrap_or_else(|| "<empty>".to_string())
        );
    }

    let parsed: S5MetricsResponse = serde_json::from_str(body)
        .with_context(|| format!("failed to parse s5 metrics response: {body}"))?;

    if parsed.peak_tps <= 0.0 {
        bail!("PeakTPS must be > 0, got {} from {}", parsed.peak_tps, url);
    }

    Ok(S5PeakTpsResult {
        url,
        status_code,
        tps: parsed.tps,
        peak_tps: parsed.peak_tps,
        peak_tps_at_ms: parsed.peak_tps_at_ms,
    })
}

async fn execute_proof_pool_send_check(
    client: &Client,
    config: &Config,
) -> Result<ProofPoolSendResult> {
    let url = format!("{}/proof-pool/send", config.base_url);
    let mut request = client.post(&url).json(&json!({ "proof_quantity": 1usize }));

    if let Some(auth_token) = config.proof_pool_auth_token.as_deref() {
        request = request.query(&[("auth_token", auth_token)]);
    }

    let response = request
        .send()
        .await
        .context("proof-pool send request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read proof-pool send response body")?;

    parse_proof_pool_send_response(status.as_u16(), &body)
}

fn parse_proof_pool_send_response(status_code: u16, body: &str) -> Result<ProofPoolSendResult> {
    if !(200..300).contains(&status_code) {
        bail!(
            "proof-pool send returned {}: {}",
            status_code,
            excerpt(body).unwrap_or_else(|| "<empty>".to_string())
        );
    }

    let parsed: ProofPoolSendResult = serde_json::from_str(body)
        .with_context(|| format!("failed to parse proof-pool send response: {body}"))?;

    if parsed.requested != 1 || parsed.flushed != 1 || parsed.accepted != 1 || parsed.rejected != 0
    {
        bail!(
            "unexpected proof-pool send result: requested={} flushed={} accepted={} rejected={}",
            parsed.requested,
            parsed.flushed,
            parsed.accepted,
            parsed.rejected
        );
    }

    Ok(parsed)
}

async fn run_tee_reset_endpoint_check(
    client: &Client,
    config: &Config,
) -> OperationCheckResult<TeeEndpointResult> {
    let started = Instant::now();
    match execute_tee_reset_endpoint_check(client, config).await {
        Ok(result) => OperationCheckResult {
            check: TEE_RESET_ENDPOINT_CHECK.to_string(),
            healthy: true,
            latency_ms: started.elapsed().as_millis(),
            result: Some(result),
            error: None,
        },
        Err(err) => OperationCheckResult {
            check: TEE_RESET_ENDPOINT_CHECK.to_string(),
            healthy: false,
            latency_ms: started.elapsed().as_millis(),
            result: None,
            error: Some(format!("{err:#}")),
        },
    }
}

async fn execute_tee_reset_endpoint_check(
    client: &Client,
    config: &Config,
) -> Result<TeeEndpointResult> {
    let url = config.tee_reset_url.clone();
    match client.post(&url).send().await {
        Ok(response) => Ok(TeeEndpointResult {
            url,
            status_code: Some(response.status().as_u16()),
        }),
        Err(err) => {
            let is_connection_refused = {
                let mut current = Some(&err as &(dyn std::error::Error + 'static));
                let mut found = false;
                while let Some(error) = current {
                    if error
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("connection refused")
                    {
                        found = true;
                        break;
                    }
                    current = error.source();
                }
                found
            };

            if err.is_timeout() || is_connection_refused {
                return Err(anyhow!("tee reset endpoint request failed: {err:#}"));
            }

            warn!(
                url = %url,
                error = format!("{err:#}"),
                "tee reset endpoint request had a non-fatal transport error; treating endpoint as alive"
            );
            Ok(TeeEndpointResult {
                url,
                status_code: None,
            })
        }
    }
}

async fn run_mcp_stress_check(config: &Config) -> OperationCheckResult<StressRunResult> {
    let started = Instant::now();
    match execute_mcp_stress_check(config).await {
        Ok(result) => OperationCheckResult {
            check: MCP_STRESS_CHECK.to_string(),
            healthy: true,
            latency_ms: started.elapsed().as_millis(),
            result: Some(result),
            error: None,
        },
        Err(err) => OperationCheckResult {
            check: MCP_STRESS_CHECK.to_string(),
            healthy: false,
            latency_ms: started.elapsed().as_millis(),
            result: None,
            error: Some(format!("{err:#}")),
        },
    }
}

async fn execute_mcp_stress_check(config: &Config) -> Result<StressRunResult> {
    let endpoint = format!("{}/mcp/mcp", config.base_url);
    let output = Command::new("sh")
        .arg(&config.stress_script_path)
        .arg("--wallets")
        .arg(config.stress_wallets.to_string())
        .arg("--txs")
        .arg(config.stress_txs.to_string())
        .arg("--endpoint")
        .arg(&endpoint)
        .env(
            "MCP_STRESS_SESSION_IDS_FILE",
            &config.stress_session_ids_file,
        )
        .env(
            "MCP_STRESS_RUST_LOG",
            env::var("MCP_STRESS_RUST_LOG")
                .unwrap_or_else(|_| "mcp_external_stress=info,rmcp=warn".to_string()),
        )
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to execute stress script at {}",
                config.stress_script_path.display()
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout_tail = tail_lines(&stdout, 20);
    let stderr_tail = tail_lines(&stderr, 20);

    if !output.status.success() {
        bail!(
            "stress script exited with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            stdout_tail,
            stderr_tail
        );
    }

    let summary = parse_stress_summary(&stdout, &stderr).ok_or_else(|| {
        anyhow!(
            "stress script did not emit a final summary line\nstdout:\n{}\nstderr:\n{}",
            stdout_tail,
            stderr_tail
        )
    })?;

    validate_stress_summary(
        &summary,
        config.stress_wallets,
        config.stress_txs,
        endpoint,
        stdout_tail,
        stderr_tail,
    )
}

fn validate_stress_summary(
    summary: &BTreeMap<String, String>,
    stress_wallets: usize,
    stress_txs: usize,
    endpoint: String,
    stdout_tail: String,
    stderr_tail: String,
) -> Result<StressRunResult> {
    let expected_sends = (stress_wallets as u64) * (stress_txs as u64);
    let ready = parse_summary_u64(summary, "ready")?;
    let sends_ok = parse_summary_u64(summary, "sends_ok")?;
    let sends_err = parse_summary_u64(summary, "sends_err")?;
    let sends_retried = summary
        .get("sends_retried")
        .map(|value| value.parse::<u64>())
        .transpose()
        .context("failed to parse sends_retried from stress summary")?;

    if ready != stress_wallets as u64 || sends_ok == 0 {
        bail!(
            "unexpected stress result: ready={} sends_ok={} sends_err={} expected_ready={} expected_sends={}\nstdout:\n{}\nstderr:\n{}",
            ready,
            sends_ok,
            sends_err,
            stress_wallets,
            expected_sends,
            stdout_tail,
            stderr_tail
        );
    }

    Ok(StressRunResult {
        wallets: stress_wallets,
        txs_per_wallet: stress_txs,
        endpoint,
        ready,
        sends_ok,
        sends_err,
        sends_retried,
        stdout_tail,
        stderr_tail,
    })
}

async fn run_disk_usage_check(
    client: &Client,
    config: &Config,
) -> OperationCheckResult<DiskUsageResult> {
    let started = Instant::now();
    match execute_disk_usage_check(client, config).await {
        Ok(result) => OperationCheckResult {
            check: DISK_USAGE_CHECK.to_string(),
            healthy: true,
            latency_ms: started.elapsed().as_millis(),
            result: Some(result),
            error: None,
        },
        Err(err) => OperationCheckResult {
            check: DISK_USAGE_CHECK.to_string(),
            healthy: false,
            latency_ms: started.elapsed().as_millis(),
            result: None,
            error: Some(format!("{err:#}")),
        },
    }
}

async fn execute_disk_usage_check(client: &Client, config: &Config) -> Result<DiskUsageResult> {
    let url = format!("{}/controller/stats", config.base_url);
    let mut request = client.get(&url);

    if let Some(username) = config.controller_basic_auth_username.as_deref() {
        request = request.basic_auth(username, config.controller_basic_auth_password.as_deref());
    }

    let response = request
        .send()
        .await
        .context("controller stats request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read controller stats response body")?;

    parse_disk_usage_response(url, &config.disk_usage_mount_path, status.as_u16(), &body)
}

fn parse_disk_usage_response(
    url: String,
    mount_path: &str,
    status_code: u16,
    body: &str,
) -> Result<DiskUsageResult> {
    if !(200..300).contains(&status_code) {
        bail!(
            "controller stats returned {}: {}",
            status_code,
            excerpt(body).unwrap_or_else(|| "<empty>".to_string())
        );
    }

    let parsed: ControllerSystemStatsResponse = serde_json::from_str(body)
        .with_context(|| format!("failed to parse controller stats response: {body}"))?;

    let available_mounts = parsed
        .disks
        .iter()
        .map(|disk| disk.mount_point.clone())
        .collect::<Vec<_>>();
    let disk = parsed
        .disks
        .into_iter()
        .find(|disk| disk.mount_point == mount_path)
        .ok_or_else(|| {
            anyhow!(
                "disk mount path {} not found in controller stats {}; available mount points: {}",
                mount_path,
                url,
                if available_mounts.is_empty() {
                    "<none>".to_string()
                } else {
                    available_mounts.join(", ")
                }
            )
        })?;

    Ok(DiskUsageResult {
        url,
        disk_name: disk.name,
        mount_path: disk.mount_point,
        usage_percent: disk.usage_percent,
        total_bytes: disk.total_bytes,
        used_bytes: disk.used_bytes,
        available_bytes: disk.available_bytes,
    })
}

fn emit_metrics(report: &MonitorReport) -> Result<()> {
    for event in build_metric_events(report)? {
        println!(
            "{}",
            serde_json::to_string(&event).context("failed to serialize metric event")?
        );
    }

    Ok(())
}

fn build_metric_events(report: &MonitorReport) -> Result<Vec<Value>> {
    let timestamp = current_timestamp_millis()?;
    let mut events = Vec::with_capacity(report.health_checks.len() + 7);

    events.push(json!({
        "_aws": {
            "Timestamp": timestamp,
            "CloudWatchMetrics": [{
                "Namespace": MONITOR_METRICS_NAMESPACE,
                "Dimensions": [["Environment"]],
                "Metrics": [
                    { "Name": "Heartbeat", "Unit": "Count" },
                    { "Name": "RunStatus", "Unit": "Count" }
                ]
            }]
        },
        "Environment": report.environment,
        "Heartbeat": 1,
        "RunStatus": metric_status_value(report.all_checks_passed),
        "RequestId": report.request_id,
        "BaseUrl": report.base_url,
    }));

    for health_check in &report.health_checks {
        events.push(build_check_metric_event(
            timestamp,
            &report.environment,
            &health_check.check,
            health_check.healthy,
            health_check.latency_ms,
            health_check.error.as_deref(),
        ));
    }

    events.push(build_check_metric_event(
        timestamp,
        &report.environment,
        &report.s5_peak_tps.check,
        report.s5_peak_tps.healthy,
        report.s5_peak_tps.latency_ms,
        report.s5_peak_tps.error.as_deref(),
    ));
    events.push(build_check_metric_event(
        timestamp,
        &report.environment,
        &report.proof_pool_send.check,
        report.proof_pool_send.healthy,
        report.proof_pool_send.latency_ms,
        report.proof_pool_send.error.as_deref(),
    ));
    events.push(build_check_metric_event(
        timestamp,
        &report.environment,
        &report.tee_reset_endpoint.check,
        report.tee_reset_endpoint.healthy,
        report.tee_reset_endpoint.latency_ms,
        report.tee_reset_endpoint.error.as_deref(),
    ));
    events.push(build_check_metric_event(
        timestamp,
        &report.environment,
        &report.mcp_stress.check,
        report.mcp_stress.healthy,
        report.mcp_stress.latency_ms,
        report.mcp_stress.error.as_deref(),
    ));
    events.push(build_check_metric_event(
        timestamp,
        &report.environment,
        &report.disk_usage.check,
        report.disk_usage.healthy,
        report.disk_usage.latency_ms,
        report.disk_usage.error.as_deref(),
    ));

    if let Some(disk_usage) = report.disk_usage.result.as_ref() {
        events.push(build_disk_usage_metric_event(
            timestamp,
            &report.environment,
            disk_usage,
        ));
    }

    Ok(events)
}

fn build_check_metric_event(
    timestamp: u64,
    environment: &str,
    check: &str,
    healthy: bool,
    latency_ms: u128,
    error: Option<&str>,
) -> Value {
    let mut event = Map::from_iter([
        (
            "_aws".to_string(),
            json!({
                "Timestamp": timestamp,
                "CloudWatchMetrics": [{
                    "Namespace": MONITOR_METRICS_NAMESPACE,
                    "Dimensions": [["Environment", "Check"]],
                    "Metrics": [
                        { "Name": "CheckStatus", "Unit": "Count" },
                        { "Name": "CheckLatencyMs", "Unit": "Milliseconds" }
                    ]
                }]
            }),
        ),
        ("Environment".to_string(), json!(environment)),
        ("Check".to_string(), json!(check)),
        (
            "CheckStatus".to_string(),
            json!(metric_status_value(healthy)),
        ),
        (
            "CheckLatencyMs".to_string(),
            json!(u64::try_from(latency_ms).unwrap_or(u64::MAX)),
        ),
    ]);

    if let Some(error) = error {
        event.insert("Error".to_string(), json!(error));
    }

    Value::Object(event)
}

fn build_disk_usage_metric_event(
    timestamp: u64,
    environment: &str,
    disk_usage: &DiskUsageResult,
) -> Value {
    json!({
        "_aws": {
            "Timestamp": timestamp,
            "CloudWatchMetrics": [{
                "Namespace": MONITOR_METRICS_NAMESPACE,
                "Dimensions": [["Environment", "MountPath"]],
                "Metrics": [
                    { "Name": "DiskUsagePercent", "Unit": "Percent" }
                ]
            }]
        },
        "Environment": environment,
        "MountPath": disk_usage.mount_path,
        "DiskUsagePercent": disk_usage.usage_percent,
        "DiskName": disk_usage.disk_name,
        "DiskTotalBytes": disk_usage.total_bytes,
        "DiskUsedBytes": disk_usage.used_bytes,
        "DiskAvailableBytes": disk_usage.available_bytes,
        "ControllerStatsUrl": disk_usage.url,
    })
}

fn failed_checks(report: &MonitorReport) -> Vec<String> {
    let mut failures = report
        .health_checks
        .iter()
        .filter(|result| !result.healthy)
        .map(|result| {
            if let Some(error) = result.error.as_deref() {
                format!("{} ({})", result.check, error)
            } else {
                format!("{} ({})", result.check, result.status_code)
            }
        })
        .collect::<Vec<_>>();

    if !report.s5_peak_tps.healthy {
        failures.push(format!(
            "{} ({})",
            report.s5_peak_tps.check,
            report
                .s5_peak_tps
                .error
                .as_deref()
                .unwrap_or("unknown error")
        ));
    }

    if !report.proof_pool_send.healthy {
        failures.push(format!(
            "{} ({})",
            report.proof_pool_send.check,
            report
                .proof_pool_send
                .error
                .as_deref()
                .unwrap_or("unknown error")
        ));
    }

    if !report.tee_reset_endpoint.healthy {
        failures.push(format!(
            "{} ({})",
            report.tee_reset_endpoint.check,
            report
                .tee_reset_endpoint
                .error
                .as_deref()
                .unwrap_or("unknown error")
        ));
    }

    if !report.mcp_stress.healthy {
        failures.push(format!(
            "{} ({})",
            report.mcp_stress.check,
            report
                .mcp_stress
                .error
                .as_deref()
                .unwrap_or("unknown error")
        ));
    }

    failures
}

fn metric_status_value(healthy: bool) -> u8 {
    if healthy {
        1
    } else {
        0
    }
}

fn current_timestamp_millis() -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?;
    Ok(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn parse_stress_summary(stdout: &str, stderr: &str) -> Option<BTreeMap<String, String>> {
    for stream in [stderr, stdout] {
        for line in stream.lines().rev() {
            if let Some(rest) = line.strip_prefix("done: ") {
                let mut values = BTreeMap::new();
                for token in rest.split_whitespace() {
                    if let Some((key, value)) = token.split_once('=') {
                        values.insert(key.to_string(), value.to_string());
                    }
                }
                return Some(values);
            }
        }
    }

    None
}

fn parse_summary_u64(values: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    values
        .get(key)
        .ok_or_else(|| anyhow!("missing {key} in stress summary"))
        .and_then(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("failed to parse {key}={value} as u64"))
        })
}

fn normalize_base_url(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

fn service_url(base_url: &str, service: &str, path: &str) -> String {
    let normalized_base = normalize_base_url(base_url);
    let normalized_path = path.strip_prefix('/').unwrap_or(path);
    if normalized_path.is_empty() {
        format!("{normalized_base}/{service}/")
    } else {
        format!("{normalized_base}/{service}/{normalized_path}")
    }
}

fn default_stress_script_path() -> PathBuf {
    match env::var_os("LAMBDA_TASK_ROOT") {
        Some(task_root) => PathBuf::from(task_root).join(DEFAULT_STRESS_SCRIPT_PATH),
        None => PathBuf::from(DEFAULT_STRESS_SCRIPT_PATH),
    }
}

fn excerpt(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        let mut excerpt = trimmed.chars().take(240).collect::<String>();
        if trimmed.chars().count() > 240 {
            excerpt.push_str("...");
        }
        Some(excerpt)
    }
}

fn tail_lines(input: &str, count: usize) -> String {
    let lines = input.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("missing required environment variable {name}"))
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_parse<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map_err(|err| anyhow!("failed to parse {name}={value}: {err}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(anyhow!("failed to read {name}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_metric_events, failed_checks, parse_disk_usage_response,
        parse_proof_pool_send_response, parse_s5_peak_tps_response, parse_stress_summary,
        service_url, validate_stress_summary, DiskUsageResult, HealthCheckResult, MonitorReport,
        OperationCheckResult, ProofPoolSendResult, S5PeakTpsResult, StressRunResult,
        TeeEndpointResult, DISK_USAGE_CHECK, MCP_STRESS_CHECK, PROOF_POOL_SEND_CHECK,
        S5_PEAK_TPS_CHECK, TEE_RESET_ENDPOINT_CHECK,
    };
    use serde_json::Value;
    use std::collections::BTreeMap;

    #[test]
    fn builds_service_urls_with_single_slashes() {
        assert_eq!(
            service_url("https://example.com/", "worker", "/health"),
            "https://example.com/worker/health"
        );
    }

    #[test]
    fn parses_final_stress_summary() {
        let summary = parse_stress_summary(
            "",
            "done: ready=10 sends_ok=50 sends_err=0 sends_retried=1 send_avg_ms=1.0",
        )
        .expect("summary should parse");

        assert_eq!(summary.get("ready").map(String::as_str), Some("10"));
        assert_eq!(summary.get("sends_ok").map(String::as_str), Some("50"));
        assert_eq!(summary.get("sends_err").map(String::as_str), Some("0"));
        assert_eq!(summary.get("sends_retried").map(String::as_str), Some("1"));
    }

    #[test]
    fn emits_heartbeat_and_check_metrics() {
        let report = sample_report();
        let events = build_metric_events(&report).expect("metric events should build");

        assert_eq!(events.len(), 15);
        assert_eq!(find_metric_value(&events[0], "Heartbeat"), Some(1));
        assert_eq!(find_metric_value(&events[0], "RunStatus"), Some(1));

        let worker_metric = events
            .iter()
            .find(|event| event["Check"] == "health:worker")
            .expect("worker metric should be present");
        assert_eq!(find_metric_value(worker_metric, "CheckStatus"), Some(1));

        let stress_metric = events
            .iter()
            .find(|event| event["Check"] == MCP_STRESS_CHECK)
            .expect("stress metric should be present");
        assert_eq!(find_metric_value(stress_metric, "CheckStatus"), Some(1));

        let disk_check_metric = events
            .iter()
            .find(|event| event["Check"] == DISK_USAGE_CHECK)
            .expect("disk usage collection metric should be present");
        assert_eq!(find_metric_value(disk_check_metric, "CheckStatus"), Some(1));

        let disk_usage_metric = events
            .iter()
            .find(|event| event["MountPath"] == "/")
            .expect("disk usage metric should be present");
        assert_eq!(
            find_metric_value_f64(disk_usage_metric, "DiskUsagePercent"),
            Some(90.25)
        );
    }

    #[test]
    fn failing_report_marks_only_failed_check_metric_as_zero() {
        let mut report = sample_report();
        report.all_checks_passed = false;
        report.health_checks[1].healthy = false;
        report.health_checks[1].status_code = 503;

        let events = build_metric_events(&report).expect("metric events should build");

        for event in events.iter().filter(|event| event.get("Check").is_some()) {
            let check = event["Check"].as_str().expect("check should be a string");
            let expected = if check == "health:worker" { 0 } else { 1 };
            assert_eq!(find_metric_value(event, "CheckStatus"), Some(expected));
        }
    }

    #[test]
    fn proof_pool_parser_rejects_malformed_response() {
        let err = parse_proof_pool_send_response(200, "{\"requested\":1,\"flushed\":0}")
            .expect_err("response should fail");

        assert!(format!("{err:#}").contains("failed to parse proof-pool send response"));
    }

    #[test]
    fn s5_peak_tps_parser_rejects_zero_peak_tps() {
        let err = parse_s5_peak_tps_response(
            "https://example.com/metrics/s5".to_string(),
            200,
            r#"{"TPS":0.12,"PeakTPS":0.0,"PeakTPSAtMs":123}"#,
        )
        .expect_err("PeakTPS == 0 should fail");

        assert!(format!("{err:#}").contains("PeakTPS must be > 0"));
    }

    #[test]
    fn disk_usage_parser_selects_requested_mount_path() {
        let result = parse_disk_usage_response(
            "http://example.com/controller/stats".to_string(),
            "/",
            200,
            r#"{
                "disks": [
                    {
                        "name": "/dev/root",
                        "mount_point": "/",
                        "total_bytes": 100,
                        "available_bytes": 10,
                        "used_bytes": 90,
                        "usage_percent": 90.0
                    },
                    {
                        "name": "/dev/data",
                        "mount_point": "/mnt/data",
                        "total_bytes": 200,
                        "available_bytes": 100,
                        "used_bytes": 100,
                        "usage_percent": 50.0
                    }
                ]
            }"#,
        )
        .expect("disk stats should parse");

        assert_eq!(result.mount_path, "/");
        assert_eq!(result.disk_name, "/dev/root");
        assert_eq!(result.used_bytes, 90);
        assert_eq!(result.usage_percent, 90.0);
    }

    #[test]
    fn disk_usage_parser_rejects_missing_mount_path() {
        let err = parse_disk_usage_response(
            "http://example.com/controller/stats".to_string(),
            "/missing",
            200,
            r#"{
                "disks": [
                    {
                        "name": "/dev/root",
                        "mount_point": "/",
                        "total_bytes": 100,
                        "available_bytes": 10,
                        "used_bytes": 90,
                        "usage_percent": 90.0
                    }
                ]
            }"#,
        )
        .expect_err("missing mount path should fail");

        assert!(format!("{err:#}").contains("available mount points: /"));
    }

    #[test]
    fn stress_validation_accepts_partial_success() {
        let mut summary = BTreeMap::new();
        summary.insert("ready".to_string(), "3".to_string());
        summary.insert("sends_ok".to_string(), "1".to_string());
        summary.insert("sends_err".to_string(), "1".to_string());

        let result = validate_stress_summary(
            &summary,
            3,
            1,
            "http://example.com/mcp/mcp".to_string(),
            "stdout".to_string(),
            "stderr".to_string(),
        )
        .expect("summary should pass with at least one successful send");

        assert_eq!(result.wallets, 3);
        assert_eq!(result.txs_per_wallet, 1);
        assert_eq!(result.ready, 3);
        assert_eq!(result.sends_ok, 1);
        assert_eq!(result.sends_err, 1);
    }

    #[test]
    fn stress_validation_rejects_zero_successes() {
        let mut summary = BTreeMap::new();
        summary.insert("ready".to_string(), "3".to_string());
        summary.insert("sends_ok".to_string(), "0".to_string());
        summary.insert("sends_err".to_string(), "3".to_string());

        let err = validate_stress_summary(
            &summary,
            3,
            1,
            "http://example.com/mcp/mcp".to_string(),
            "stdout".to_string(),
            "stderr".to_string(),
        )
        .expect_err("summary should fail when no send succeeds");

        assert!(format!("{err:#}").contains("unexpected stress result"));
    }

    #[test]
    fn failed_checks_collects_all_failed_components() {
        let mut report = sample_report();
        report.all_checks_passed = false;
        report.health_checks[2].healthy = false;
        report.health_checks[2].status_code = 500;
        report.proof_pool_send.healthy = false;
        report.proof_pool_send.result = None;
        report.proof_pool_send.error = Some("bad response".to_string());

        let failures = failed_checks(&report);

        assert!(failures.iter().any(|item| item == "health:fvk (500)"));
        assert!(failures
            .iter()
            .any(|item| item == "proof-pool:send (bad response)"));
    }

    fn sample_report() -> MonitorReport {
        MonitorReport {
            request_id: "request-123".to_string(),
            environment: "midnight-l2-testnet".to_string(),
            base_url: "http://172.33.91.192".to_string(),
            duration_ms: 100,
            all_checks_passed: true,
            health_checks: vec![
                sample_health("health:rollup", "rollup"),
                sample_health("health:worker", "worker"),
                sample_health("health:fvk", "fvk"),
                sample_health("health:indexer", "indexer"),
                sample_health("health:mcp", "mcp"),
                sample_health("health:metrics", "metrics"),
                sample_health("health:oracle", "oracle"),
                sample_health("health:proof-pool", "proof-pool"),
            ],
            s5_peak_tps: OperationCheckResult {
                check: S5_PEAK_TPS_CHECK.to_string(),
                healthy: true,
                latency_ms: 8,
                result: Some(S5PeakTpsResult {
                    url: "http://172.33.91.192/metrics/s5".to_string(),
                    status_code: 200,
                    tps: 3.25,
                    peak_tps: 4.5,
                    peak_tps_at_ms: Some(1_700_000_000_000),
                }),
                error: None,
            },
            proof_pool_send: OperationCheckResult {
                check: PROOF_POOL_SEND_CHECK.to_string(),
                healthy: true,
                latency_ms: 10,
                result: Some(ProofPoolSendResult {
                    requested: 1,
                    flushed: 1,
                    accepted: 1,
                    rejected: 0,
                    ready_proofs: 100,
                }),
                error: None,
            },
            tee_reset_endpoint: OperationCheckResult {
                check: TEE_RESET_ENDPOINT_CHECK.to_string(),
                healthy: true,
                latency_ms: 12,
                result: Some(TeeEndpointResult {
                    url: "http://74.235.106.62:9898/reset".to_string(),
                    status_code: Some(405),
                }),
                error: None,
            },
            mcp_stress: OperationCheckResult {
                check: MCP_STRESS_CHECK.to_string(),
                healthy: true,
                latency_ms: 25,
                result: Some(StressRunResult {
                    wallets: 3,
                    txs_per_wallet: 1,
                    endpoint: "http://172.33.91.192/mcp/mcp".to_string(),
                    ready: 3,
                    sends_ok: 3,
                    sends_err: 0,
                    sends_retried: Some(0),
                    stdout_tail: "done".to_string(),
                    stderr_tail: String::new(),
                }),
                error: None,
            },
            disk_usage: OperationCheckResult {
                check: DISK_USAGE_CHECK.to_string(),
                healthy: true,
                latency_ms: 6,
                result: Some(DiskUsageResult {
                    url: "http://172.33.91.192/controller/stats".to_string(),
                    disk_name: "/dev/root".to_string(),
                    mount_path: "/".to_string(),
                    usage_percent: 90.25,
                    total_bytes: 1_000,
                    used_bytes: 902,
                    available_bytes: 98,
                }),
                error: None,
            },
        }
    }

    fn sample_health(check: &str, service: &str) -> HealthCheckResult {
        HealthCheckResult {
            check: check.to_string(),
            service: service.to_string(),
            url: format!("http://172.33.91.192/{service}/health"),
            healthy: true,
            status_code: 200,
            latency_ms: 5,
            body_excerpt: None,
            error: None,
        }
    }

    fn find_metric_value(event: &Value, key: &str) -> Option<u64> {
        event.get(key).and_then(Value::as_u64)
    }

    fn find_metric_value_f64(event: &Value, key: &str) -> Option<f64> {
        event.get(key).and_then(Value::as_f64)
    }
}
