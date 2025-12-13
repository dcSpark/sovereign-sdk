//! Node and environment management for e2e_runner.
//!
//! Handles spawning an ephemeral local node or connecting to external services.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::ChildGuard;

use super::RunnerConfig;

/// Configuration for connecting to external services.
#[derive(Clone, Debug)]
pub(crate) struct ExternalConfig {
    pub(crate) node_url: String,
    pub(crate) verifier_url: String,
}

impl ExternalConfig {
    /// Create from RunnerConfig if both URLs are provided.
    pub(crate) fn from_config(config: &RunnerConfig) -> Result<Option<Self>> {
        match (&config.external_node_url, &config.external_verifier_url) {
            (Some(node_url), Some(verifier_url)) => Ok(Some(Self {
                node_url: node_url.clone(),
                verifier_url: verifier_url.clone(),
            })),
            (None, None) => Ok(None),
            _ => anyhow::bail!(
                "Both external node and verifier URLs must be provided to use external services"
            ),
        }
    }
}

/// Environment for running the benchmark.
pub(crate) struct TestEnvironment {
    pub(crate) api_url: String,
    pub(crate) da_connection_string: Option<String>,
    #[allow(dead_code)]
    pub(crate) temp_dir: Option<tempfile::TempDir>,
    pub(crate) child_guard: Option<ChildGuard>,
}

impl TestEnvironment {
    /// Create environment pointing to external services.
    pub(crate) fn external(api_url: String) -> Self {
        Self {
            api_url,
            da_connection_string: None,
            temp_dir: None,
            child_guard: None,
        }
    }

    /// Shutdown the managed node (if any).
    pub(crate) fn shutdown(&mut self) {
        if let Some(mut guard) = self.child_guard.take() {
            let _ = guard.0.kill();
            let _ = guard.0.wait();
        }
    }
}

/// Rewrite config file for ephemeral test environment.
fn make_temp_config(base_config: &str, data_dir: &std::path::Path, http_port: u16) -> String {
    let da_conn = format!(
        "connection_string = \"sqlite://{}/da.sqlite?mode=rwc\"",
        data_dir.display()
    );
    let storage_path = format!("path = \"{}\"", data_dir.display());
    let bind_port = format!("bind_port = {}", http_port);

    let mut out = String::with_capacity(base_config.len() + 256);
    for line in base_config.lines() {
        let l = line.trim_start();
        if l.starts_with("connection_string = ") {
            out.push_str(&da_conn);
        } else if l.starts_with("path = ") && !l.contains("target/") {
            out.push_str(&storage_path);
        } else if l.starts_with("bind_port = ") {
            out.push_str(&bind_port);
        } else if l.starts_with("finalization = ") {
            // Speed up readiness for tests
            out.push_str("finalization = 0");
        } else if l.starts_with("block_time_ms = ") {
            // Faster mock DA blocks
            out.push_str("block_time_ms = 200");
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Spawn an ephemeral local node (or connect to external services).
pub(crate) fn prepare_environment(
    crate_dir: &Path,
    bin_path: &str,
    program_path: &str,
    verifier_bin: &str,
    prover_bin: &str,
    shader_dir: &str,
    external: Option<ExternalConfig>,
) -> Result<TestEnvironment> {
    if let Some(cfg) = external {
        return Ok(TestEnvironment::external(cfg.node_url));
    }

    let base_cfg_path = crate_dir.join("rollup_config.toml");
    let base_cfg = std::fs::read_to_string(&base_cfg_path)
        .with_context(|| format!("Failed to read base config at {}", base_cfg_path.display()))?;

    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let http_port = listener.local_addr()?.port();
    drop(listener);

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("demo_data");
    std::fs::create_dir_all(&data_dir)?;
    let new_cfg = make_temp_config(&base_cfg, &data_dir, http_port);
    let cfg_path = temp.path().join("rollup_config.toml");
    std::fs::write(&cfg_path, new_cfg)?;

    let mut child = Command::new(bin_path)
        .current_dir(crate_dir)
        .arg("--rollup-config-path")
        .arg(cfg_path.as_os_str())
        .arg("--prometheus-exporter-bind")
        .arg("127.0.0.1:0")
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .env("LIGERO_PROGRAM_PATH", program_path)
        .env("LIGERO_VERIFIER_BIN", verifier_bin)
        .env("LIGERO_PROVER_BIN", prover_bin)
        .env("LIGERO_SHADER_PATH", shader_dir)
        .env("LIGERO_PACKING", "8192")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sov-rollup-ligero")?;

    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                eprintln!("[node stdout] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                eprintln!("[node stderr] {}", line);
            }
        });
    }

    let api_url = format!("http://127.0.0.1:{}", http_port);
    let da_connection_string = format!("sqlite://{}/da.sqlite?mode=rwc", data_dir.display());

    Ok(TestEnvironment {
        api_url,
        da_connection_string: Some(da_connection_string),
        temp_dir: Some(temp),
        child_guard: Some(ChildGuard::new(child)),
    })
}
