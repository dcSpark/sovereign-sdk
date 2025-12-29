//! Test utilities for Ligero proof generation

use std::env;
use std::path::PathBuf;

use crate::ligero::Ligero;

fn env_path(var: &str, default_rel: &str) -> PathBuf {
    if let Ok(val) = env::var(var) {
        PathBuf::from(val)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default_rel)
    }
}

/// Helper function to get the platform-specific binary directory
#[allow(dead_code)]
pub fn get_platform_bin_dir() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos-arm64"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        panic!("Unsupported platform for Ligero tests");
    }
}

/// Helper function to create a Ligero instance for testing
#[allow(dead_code)]
pub fn create_test_ligero() -> Option<Ligero> {
    let platform_dir = get_platform_bin_dir();

    // Defaults are best-effort; callers can override with env vars pointing to local Ligero assets.
    let prover = env_path(
        "LIGERO_PROVER_BINARY_PATH",
        &format!("../adapters/ligero/bins/{}/bin/webgpu_prover", platform_dir),
    );
    let shader = env_path("LIGERO_SHADER_PATH", "../adapters/ligero/bins/shader");
    let program = env_path(
        "LIGERO_PROGRAM_PATH",
        "../adapters/ligero/guest/bins/programs/note_spend_guest.wasm",
    );

    for (label, path) in [("prover", &prover), ("shader", &shader), ("program", &program)] {
        if !path.exists() {
            eprintln!(
                "⚠️  Skipping Ligero tests: {} path not found at {}",
                label,
                path.display()
            );
            return None;
        }
    }

    Some(Ligero::new(Some(prover), Some(shader), Some(program)))
}
