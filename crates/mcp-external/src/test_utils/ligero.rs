//! Test utilities for Ligero proof generation

use std::env;

use crate::ligero::Ligero;

/// Helper function to create a Ligero instance for testing
#[allow(dead_code)]
pub fn create_test_ligero() -> Option<Ligero> {
    let program =
        env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());
    let proof_service_url = env::var("LIGERO_PROOF_SERVICE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:1313".to_string());

    if program.trim().is_empty() || proof_service_url.trim().is_empty() {
        eprintln!(
            "⚠️  Skipping Ligero tests: missing LIGERO_PROGRAM_PATH or LIGERO_PROOF_SERVICE_URL"
        );
        return None;
    }

    Some(Ligero::new(proof_service_url, program))
}
