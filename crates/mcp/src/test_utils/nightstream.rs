//! Test utilities for Nightstream proof generation

use crate::nightstream::Nightstream;

/// Helper function to create a Nightstream instance for testing
#[allow(dead_code)]
pub fn create_test_nightstream() -> Option<Nightstream> {
    let program =
        std::env::var("NIGHTSTREAM_PROGRAM_PATH")
            .unwrap_or_else(|_| std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string()));
    let proof_service_url = std::env::var("NIGHTSTREAM_PROOF_SERVICE_URL")
        .or_else(|_| std::env::var("LIGERO_PROOF_SERVICE_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());

    if program.trim().is_empty() || proof_service_url.trim().is_empty() {
        eprintln!(
            "⚠️  Skipping Nightstream tests: missing NIGHTSTREAM_PROGRAM_PATH or NIGHTSTREAM_PROOF_SERVICE_URL"
        );
        return None;
    }

    Some(Nightstream::new(proof_service_url, program))
}
