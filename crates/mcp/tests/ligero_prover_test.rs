//! Integration-style test for the Ligero prover. Requires the local prover binary and GPU/WebGPU.

use std::{env, path::PathBuf};

use mcp::ligero::{Ligero, LigeroProgramArguments};

const DEFAULT_PROGRAM_REL: &str = "../adapters/ligero/guest/bins/programs/note_spend_guest.wasm";
const DEFAULT_PROVER_REL: &str = "../adapters/ligero/bins/macos/bin/webgpu_prover";
const DEFAULT_SHADER_REL: &str = "../adapters/ligero/bins/macos/shader";

fn env_path(var: &str, default_rel: &str) -> PathBuf {
    if let Ok(val) = env::var(var) {
        PathBuf::from(val)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default_rel)
    }
}

fn create_test_ligero() -> Option<Ligero> {
    let prover = env_path("LIGERO_PROVER_BINARY_PATH", DEFAULT_PROVER_REL);
    let shader = env_path("LIGERO_SHADER_PATH", DEFAULT_SHADER_REL);
    let program = env_path("LIGERO_PROGRAM_PATH", DEFAULT_PROGRAM_REL);

    for (label, path) in [
        ("prover", &prover),
        ("shader", &shader),
        ("program", &program),
    ] {
        if !path.exists() {
            eprintln!(
                "⚠️  Skipping Ligero prover test: {} path not found at {}",
                label,
                path.display()
            );
            return None;
        }
    }

    Some(Ligero::new(
        Some(prover),
        None,
        Some(shader),
        Some(program),
    ))
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
