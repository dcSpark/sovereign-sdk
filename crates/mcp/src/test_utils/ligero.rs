//! Test utilities for Ligero proof generation

use std::env;
use std::path::PathBuf;

use crate::ligero::Ligero;
use ligero_runner::LigeroRunner;

#[allow(dead_code)]
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
    // Pass a circuit name (or a full `.wasm` path) via LIGERO_PROGRAM_PATH.
    let program = env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());
    let program_path = match ligero_runner::resolve_program(&program) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("⚠️  Skipping Ligero tests: failed to resolve program '{program}': {e}");
            return None;
        }
    };

    // Create the runner using the program *specifier* (name or path). `ligero-runner` resolves internally.
    let runner = LigeroRunner::new(&program);
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

    Some(Ligero::new(Some(prover), Some(shader), Some(program_path)))
}
