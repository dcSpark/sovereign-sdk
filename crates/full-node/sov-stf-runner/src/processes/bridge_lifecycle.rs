//! Manages the L1 Bridge contract lifecycle: auto-deploy on genesis and
//! executor service spawning / shutdown.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tracing::{info, warn};

const BRIDGE_ADDRESS_FILE: &str = "bridge_contract_address";
const PENDING_FINALIZE_FILE: &str = "pending_finalize.json";

/// Deploy the Bridge contract via bridge-cli and return the contract address.
///
/// Spawns `npx tsx src/cli.ts deploy ...` as a subprocess and parses the JSON
/// output for `contractAddress`.
pub async fn deploy_bridge(
    cli_path: &Path,
    network: &str,
    funding_seed: &str,
    genesis_state_root_hex: &str,
    genesis_batch_hash_hex: &str,
    rollup_id_hex: Option<&str>,
) -> Result<String> {
    let cli_path = std::fs::canonicalize(cli_path)
        .with_context(|| format!("bridge_cli_path not found: {}", cli_path.display()))?;

    let mut args = vec![
        "--yes".to_string(),
        "tsx".to_string(),
        "src/cli.ts".to_string(),
        "deploy".to_string(),
        "-n".to_string(),
        network.to_string(),
        "--deployer-seed".to_string(),
        funding_seed.to_string(),
        "--genesis-state-root".to_string(),
        genesis_state_root_hex.to_string(),
        "--genesis-batch-hash".to_string(),
        genesis_batch_hash_hex.to_string(),
        "--json".to_string(),
    ];
    if let Some(rid) = rollup_id_hex {
        args.push("--rollup-id".to_string());
        args.push(rid.to_string());
    }

    info!(
        cli_dir = %cli_path.display(),
        network,
        genesis_state_root = genesis_state_root_hex,
        "Deploying L1 Bridge contract via bridge-cli (this may take a while)..."
    );

    let output = Command::new("npx")
        .args(&args)
        .current_dir(&cli_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn bridge-cli deploy process")?
        .wait_with_output()
        .await
        .context("bridge-cli deploy process failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "bridge-cli deploy failed (exit {})\nstdout: {}\nstderr: {}",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // bridge-cli prints log lines before the JSON; find the last line that looks like JSON.
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .with_context(|| {
            format!(
                "No JSON object found in bridge-cli deploy output:\n{}",
                stdout.trim()
            )
        })?;
    let json: serde_json::Value = serde_json::from_str(json_line.trim()).with_context(|| {
        format!(
            "Failed to parse bridge-cli deploy JSON: {}",
            json_line.trim()
        )
    })?;

    let address = json
        .get("contractAddress")
        .and_then(|v| v.as_str())
        .context("bridge-cli deploy output missing 'contractAddress' field")?
        .to_string();

    info!(contract_address = %address, "L1 Bridge contract deployed successfully");
    Ok(address)
}

/// Spawn the bridge-cli executor service as a child process.
///
/// Returns the child handle; the caller is responsible for killing it on shutdown.
pub async fn start_executor(
    cli_path: &Path,
    network: &str,
    contract_address: &str,
    port: u16,
    funding_seed: &str,
) -> Result<Child> {
    let cli_path = std::fs::canonicalize(cli_path)
        .with_context(|| format!("bridge_cli_path not found: {}", cli_path.display()))?;

    info!(
        contract_address,
        port,
        cli_dir = %cli_path.display(),
        "Starting managed executor service..."
    );

    let child = Command::new("npx")
        .args(["--yes", "tsx", "src/executor-server.ts"])
        .current_dir(&cli_path)
        .env("BRIDGE_CONTRACT_ADDRESS", contract_address)
        .env("MIDNIGHT_NETWORK", network)
        .env("EXECUTOR_PORT", port.to_string())
        .env("FUNDING_SEED", funding_seed)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn executor service process")?;

    info!(pid = child.id(), "Executor service process spawned");
    Ok(child)
}

/// Persist the Bridge contract address to the rollup's storage directory.
pub fn persist_contract_address(storage_path: &Path, address: &str) -> Result<()> {
    let file = storage_path.join(BRIDGE_ADDRESS_FILE);
    std::fs::write(&file, address)
        .with_context(|| format!("Failed to write bridge address to {}", file.display()))?;
    info!(path = %file.display(), address, "Persisted Bridge contract address");
    Ok(())
}

/// Load a previously persisted Bridge contract address from the rollup's storage directory.
pub fn load_contract_address(storage_path: &Path) -> Option<String> {
    let file = storage_path.join(BRIDGE_ADDRESS_FILE);
    match std::fs::read_to_string(&file) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                info!(path = %file.display(), address = %trimmed, "Loaded persisted Bridge contract address");
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

/// Resolve the Bridge contract address from configuration or persisted state.
pub fn resolve_contract_address(
    config_address: Option<&str>,
    storage_path: &Path,
) -> Option<String> {
    config_address
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| load_contract_address(storage_path))
}

/// Wait for the executor service to become ready by polling its `/state` endpoint.
pub async fn wait_for_executor_ready(base_url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build health-check HTTP client")?;

    let url = format!("{}/state", base_url.trim_end_matches('/'));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(3);

    info!(url = %url, timeout_secs = timeout.as_secs(), "Waiting for executor service to become ready...");

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("Executor service is ready");
                return Ok(());
            }
            Ok(resp) => {
                warn!(status = %resp.status(), "Executor not ready yet");
            }
            Err(_) => {}
        }

        if tokio::time::Instant::now() + interval > deadline {
            anyhow::bail!(
                "Executor service at {} did not become ready within {}s",
                base_url,
                timeout.as_secs()
            );
        }

        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(max_interval);
    }
}

/// Build the executor base URL from a port number.
pub fn executor_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// Resolve the absolute path for bridge_cli_path, relative to the config file directory.
pub fn resolve_bridge_cli_path(bridge_cli_path: &Path, config_dir: &Path) -> PathBuf {
    if bridge_cli_path.is_absolute() {
        bridge_cli_path.to_path_buf()
    } else {
        config_dir.join(bridge_cli_path)
    }
}

// ---------------------------------------------------------------------------
// Pending-finalize persistence (crash recovery for committed-but-not-finalized
// batches).  The file is written atomically (write tmp + rename) so a crash
// mid-write never leaves a corrupt file.
// ---------------------------------------------------------------------------

/// Data persisted after a successful `commitBatch` so the batch can be
/// finalized on restart if the rollup crashes before `finalizeBatch` completes.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PendingFinalize {
    /// The L1 batch index that was committed.
    pub batch_index: u64,
    /// The BatchPublicDataV1Full JSON as expected by the executor.
    pub batch_public_data_json: String,
    /// Hex-encoded rollup ID (32 bytes).
    pub rollup_id_hex: String,
}

/// Persist the pending-finalize payload to disk (atomic write).
pub fn persist_pending_finalize(storage_path: &Path, data: &PendingFinalize) -> Result<()> {
    let target = storage_path.join(PENDING_FINALIZE_FILE);
    let tmp = storage_path.join(format!("{PENDING_FINALIZE_FILE}.tmp"));
    let json = serde_json::to_string_pretty(data)
        .context("Failed to serialize pending_finalize data")?;
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), target.display()))?;
    info!(
        batch_index = data.batch_index,
        path = %target.display(),
        "Persisted pending-finalize data"
    );
    Ok(())
}

/// Remove the pending-finalize file after successful finalization.
pub fn remove_pending_finalize(storage_path: &Path) {
    let target = storage_path.join(PENDING_FINALIZE_FILE);
    match std::fs::remove_file(&target) {
        Ok(()) => info!(path = %target.display(), "Removed pending-finalize file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %target.display(), error = %e, "Failed to remove pending-finalize file"),
    }
}

/// Load a previously persisted pending-finalize payload, if any.
pub fn load_pending_finalize(storage_path: &Path) -> Option<PendingFinalize> {
    let target = storage_path.join(PENDING_FINALIZE_FILE);
    match std::fs::read_to_string(&target) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(pf) => Some(pf),
            Err(e) => {
                warn!(path = %target.display(), error = %e, "Corrupt pending-finalize file; ignoring");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(path = %target.display(), error = %e, "Error reading pending-finalize file");
            None
        }
    }
}

/// Attempt to complete a pending finalize from a previous run.
///
/// Returns `true` if a pending batch was successfully finalized, `false` if
/// there was nothing to do, and an error if finalization failed.
pub async fn recover_pending_finalize(
    storage_path: &Path,
    executor: &super::ExecutorClient,
) -> Result<bool> {
    use super::executor_client::ExecutorBridgeState;

    let pending = match load_pending_finalize(storage_path) {
        Some(pf) => pf,
        None => return Ok(false),
    };

    let state: ExecutorBridgeState = executor
        .get_state()
        .await
        .context("Failed to query executor /state for pending-finalize recovery")?;

    if state.last_committed_batch_index <= state.last_finalized_batch_index {
        info!(
            committed = state.last_committed_batch_index,
            finalized = state.last_finalized_batch_index,
            "No committed-but-not-finalized gap on L1; removing stale pending-finalize file"
        );
        remove_pending_finalize(storage_path);
        return Ok(false);
    }

    info!(
        pending_batch_index = pending.batch_index,
        l1_committed = state.last_committed_batch_index,
        l1_finalized = state.last_finalized_batch_index,
        "Recovering committed-but-not-finalized batch from previous run"
    );

    const SIGNATURE_MAX_NONCE: u64 = 256;
    const SIGNER_BITMAP: u8 = 0b111;
    const FINALIZE_TIMESTAMP: u64 = 0;

    let signatures_json = executor
        .build_signatures(&pending.batch_public_data_json, SIGNATURE_MAX_NONCE)
        .await
        .context("build_signatures failed during pending-finalize recovery")?;

    executor
        .finalize_batch(
            &pending.batch_public_data_json,
            &signatures_json,
            SIGNER_BITMAP,
            FINALIZE_TIMESTAMP,
        )
        .await
        .context("finalize_batch failed during pending-finalize recovery")?;

    info!(
        batch_index = pending.batch_index,
        "Successfully finalized pending batch from previous run"
    );
    remove_pending_finalize(storage_path);
    Ok(true)
}
