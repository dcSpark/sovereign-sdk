use anyhow::Result;
use sov_node_client::NodeClient;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;

/// Locate the built sov-rollup-ligero binary using common env vars or target dir heuristics.
pub fn find_rollup_binary() -> Result<String> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_sov-rollup-ligero") {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_sov_rollup_ligero") {
        return Ok(p);
    }

    let target_dir = std::env::var("CARGO_TARGET_DIR").ok().unwrap_or_else(|| {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .ancestors()
            .find(|p| p.join("Cargo.toml").exists() && p.join("examples").exists())
            .unwrap_or_else(|| manifest_dir.as_path());
        repo_root.join("target").to_string_lossy().to_string()
    });

    let target_path = Path::new(&target_dir);
    for profile in &["release", "debug"] {
        let candidate = target_path.join(profile).join("sov-rollup-ligero");
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().to_string());
        }
    }

    anyhow::bail!(
        "sov-rollup-ligero binary not found in target/{{release,debug}}; \
         run `cargo build -p sov-rollup-ligero` or `cargo build -p sov-rollup-ligero --release`."
    );
}

/// Poll the node's readiness endpoint until it becomes healthy or times out.
pub async fn wait_for_ready(client: &NodeClient, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for rollup to be ready");
        }
        if client.client.is_ready().await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
}
