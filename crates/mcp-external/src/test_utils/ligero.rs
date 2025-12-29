//! Test utilities for Ligero proof generation

use std::env;
use std::path::PathBuf;

use crate::ligero::Ligero;
use ligero_webgpu_runner::LigeroRunner;

fn env_path(var: &str, default_rel: &str) -> PathBuf {
    if let Ok(val) = env::var(var) {
        PathBuf::from(val)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default_rel)
    }
}

fn env_opt(var: &str) -> Option<PathBuf> {
    env::var(var).ok().map(PathBuf::from)
}

/// Helper function to create a Ligero instance for testing
#[allow(dead_code)]
pub fn create_test_ligero() -> Option<Ligero> {
    let program = env_path(
        "LIGERO_PROGRAM_PATH",
        "../adapters/ligero/guest/bins/programs/note_spend_guest.wasm",
    );

    if !program.exists() {
        eprintln!(
            "⚠️  Skipping Ligero tests: program path not found at {}",
            program.display()
        );
        return None;
    }

    // Prefer env overrides, otherwise use ligero-webgpu-runner's built-in discovery (git checkout / LIGERO_ROOT).
    let runner = LigeroRunner::new(&program.to_string_lossy());
    let prover = env_opt("LIGERO_PROVER_BIN")
        .or_else(|| env_opt("LIGERO_PROVER_BINARY_PATH"))
        .unwrap_or_else(|| runner.paths().prover_bin.clone());
    let shader = env_opt("LIGERO_SHADER_PATH")
        .unwrap_or_else(|| PathBuf::from(runner.config().shader_path.clone()));

    for (label, path) in [("prover", &prover), ("shader", &shader)] {
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
