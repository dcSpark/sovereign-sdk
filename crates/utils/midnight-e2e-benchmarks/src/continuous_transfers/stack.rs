//! Managed stack for continuous_transfers.
//!
//! Handles starting a managed rollup node with demo_data and connecting to it.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tempfile::TempDir;
use toml::Value as TomlValue;

use sov_node_client::NodeClient;

use crate::{
    find_rollup_binary, start_local_verifier, wait_for_ready, ChildGuard, LigeroEnv,
};
use crate::bench_shared::{rollup_crate_dir, fetch_chain_hash};

use super::ContinuousConfig;

/// Prompt user to confirm wiping demo_data directory.
pub(crate) fn confirm_and_wipe_demo_data(crate_dir: &Path) -> Result<()> {
    let demo_data = crate_dir.join("demo_data");
    if !demo_data.exists() {
        return Ok(());
    }

    let skip_confirm = std::env::var("MANAGED_MODE_SKIP_CONFIRM")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);

    eprintln!(
        "[managed-mode] This will DELETE all data under {}",
        demo_data.display()
    );

    if !skip_confirm {
        eprint!("Type 'yes' to continue (anything else aborts): ");
        io::stdout().flush().ok();

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read confirmation")?;
        let trimmed = input.trim().to_ascii_lowercase();
        if trimmed != "yes" {
            bail!("Aborted by user; demo_data preserved");
        }
    } else {
        eprintln!("[managed-mode] MANAGED_MODE_SKIP_CONFIRM=1 set, proceeding without confirmation");
    }

    fs::remove_dir_all(&demo_data)
        .with_context(|| format!("Failed to delete {}", demo_data.display()))?;
    fs::create_dir_all(&demo_data)
        .with_context(|| format!("Failed to recreate {}", demo_data.display()))?;

    Ok(())
}

/// A managed node + verifier stack.
pub(crate) struct ManagedStack {
    pub(crate) api_url: String,
    pub(crate) verifier_url: String,
    pub(crate) chain_hash: [u8; 32],
    pub(crate) _temp_dir: TempDir,
    pub(crate) _child_guard: ChildGuard,
}

/// Rewrite config for managed mode.
fn make_temp_config(base_config: &str, crate_dir: &Path) -> String {
    let da_conn = format!(
        "connection_string = \"sqlite://{}/demo_data/da.sqlite?mode=rwc\"",
        crate_dir.display()
    );

    let mut out = String::with_capacity(base_config.len() + 64);
    for line in base_config.lines() {
        let l = line.trim_start();
        if l.starts_with("connection_string = ") {
            out.push_str(&da_conn);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Start a managed rollup node and verifier service.
pub(crate) async fn start_managed_stack(
    ligero_env: &LigeroEnv,
    config: &ContinuousConfig,
) -> Result<ManagedStack> {
    let crate_dir = rollup_crate_dir()?;
    let bin_path = find_rollup_binary()?;

    let base_cfg_path = crate_dir.join("rollup_config.toml");
    let base_cfg = std::fs::read_to_string(&base_cfg_path)
        .with_context(|| format!("Failed to read base config at {}", base_cfg_path.display()))?;

    let temp = tempfile::tempdir()?;
    let new_cfg = make_temp_config(&base_cfg, &crate_dir);
    let cfg_path = temp.path().join("rollup_config.toml");
    std::fs::write(&cfg_path, new_cfg)?;

    // Parse bind_host/bind_port from the base config
    let cfg_value: TomlValue = toml::from_str(&base_cfg)
        .with_context(|| "Failed to parse rollup_config.toml for managed mode")?;
    let bind_host = cfg_value
        .get("runner")
        .and_then(|r| r.get("http_config"))
        .and_then(|h| h.get("bind_host"))
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    let bind_port = cfg_value
        .get("runner")
        .and_then(|r| r.get("http_config"))
        .and_then(|h| h.get("bind_port"))
        .and_then(|v| v.as_integer())
        .unwrap_or(12346);

    let da_connection_string = format!(
        "sqlite://{}/demo_data/da.sqlite?mode=rwc",
        crate_dir.display()
    );

    let mut child = Command::new(bin_path)
        .current_dir(&crate_dir)
        .arg("--rollup-config-path")
        .arg(cfg_path.as_os_str())
        .arg("--prometheus-exporter-bind")
        .arg("127.0.0.1:0")
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .env("LIGERO_PROGRAM_PATH", &ligero_env.program_path)
        .env("LIGERO_VERIFIER_BIN", &ligero_env.verifier_bin)
        .env("LIGERO_PROVER_BIN", &ligero_env.prover_bin)
        .env("LIGERO_SHADER_PATH", &ligero_env.shader_dir)
        .env("LIGERO_PACKING", "8192")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sov-rollup-ligero")?;

    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                eprintln!("[node stdout] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                eprintln!("[node stderr] {}", line);
            }
        });
    }

    let api_host = if bind_host == "0.0.0.0" { "127.0.0.1" } else { bind_host };
    let api_url = format!("http://{}:{}", api_host, bind_port);

    let client = NodeClient::new_unchecked(&api_url);
    wait_for_ready(&client, Duration::from_secs(90)).await?;

    // Fetch chain hash using shared helper
    let chain_hash = fetch_chain_hash(&client).await?;

    let verifier_parallelism = std::cmp::max(4, config.max_concurrent_proofs);
    let verifier_url = start_local_verifier(
        &api_url,
        ligero_env.method_id,
        &da_connection_string,
        verifier_parallelism,
        false,
    )
    .await?;

    Ok(ManagedStack {
        api_url,
        verifier_url,
        chain_hash,
        _temp_dir: temp,
        _child_guard: ChildGuard::new(child),
    })
}
