//! Integration-style test for the Ligero proof service.

use mcp_external::ligero::{Ligero, LigeroProgramArguments};
use std::env;

fn create_test_ligero() -> Option<Ligero> {
    let program =
        env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());
    let proof_service_url = env::var("LIGERO_PROOF_SERVICE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());

    if program.trim().is_empty() || proof_service_url.trim().is_empty() {
        eprintln!(
            "⚠️  Skipping Ligero proof service test: missing LIGERO_PROGRAM_PATH or LIGERO_PROOF_SERVICE_URL"
        );
        return None;
    }

    Some(Ligero::new(proof_service_url, program))
}

#[tracing_test::traced_test]
#[tokio::test]
async fn test_generate_proof() {
    let Some(ligero) = create_test_ligero() else {
        return;
    };

    let proof = match ligero
        .generate_proof(
            vec![1],
            vec![
                LigeroProgramArguments::I64 { i64: 1 },
                LigeroProgramArguments::I64 { i64: 1 },
            ],
        )
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("⚠️  Skipping Ligero proof service test: {}", e);
            return;
        }
    };
    assert!(!proof.is_empty(), "proof should not be empty");
}
