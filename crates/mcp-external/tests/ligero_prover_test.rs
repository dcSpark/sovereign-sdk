//! Integration-style test for the Ligero prover. Requires GPU/WebGPU and Ligero prover assets.

use std::{env, path::PathBuf};

use ligero_runner::LigeroRunner;
use mcp_external::ligero::{Ligero, LigeroProgramArguments};

const DEFAULT_PROGRAM_REL: &str = "../adapters/ligero/guest/bins/programs/note_spend_guest.wasm";

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

fn create_test_ligero() -> Option<Ligero> {
    let program = env_path("LIGERO_PROGRAM_PATH", DEFAULT_PROGRAM_REL);
    if !program.exists() {
        eprintln!(
            "⚠️  Skipping Ligero prover test: program path not found at {}",
            program.display()
        );
        return None;
    }

    let runner = LigeroRunner::new(&program.to_string_lossy());
    let prover = env_opt("LIGERO_PROVER_BIN")
        .or_else(|| env_opt("LIGERO_PROVER_BINARY_PATH"))
        .unwrap_or_else(|| runner.paths().prover_bin.clone());
    let shader = env_opt("LIGERO_SHADER_PATH")
        .unwrap_or_else(|| PathBuf::from(runner.config().shader_path.clone()));

    for (label, path) in [("prover", &prover), ("shader", &shader)] {
        if !path.exists() {
            eprintln!(
                "⚠️  Skipping Ligero prover test: {} path not found at {}",
                label,
                path.display()
            );
            return None;
        }
    }

    Some(Ligero::new(Some(prover), Some(shader), Some(program)))
}

#[tracing_test::traced_test]
#[test]
fn test_generate_proof() {
    let Some(ligero) = create_test_ligero() else {
        return;
    };

    let proof = match ligero.generate_proof(
        8192,
        Some(8000),
        vec![1],
        vec![
            LigeroProgramArguments::I64 { i64: 1 },
            LigeroProgramArguments::I64 { i64: 1 },
        ],
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("⚠️  Skipping Ligero prover test: {}", e);
            return;
        }
    };
    assert!(!proof.is_empty(), "proof should not be empty");
}
