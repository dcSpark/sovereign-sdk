//! Test utilities module
//!
//! This module provides common utilities for testing across the MCP crate.

pub mod ligero;

use crate::provider::Provider;

/// Test wallet key for use in integration tests
///
/// This key corresponds to the test wallet defined in test-data/keys/token_deployer_private_key.json
/// and is used across multiple test files for consistency.
pub const TEST_PRIVATE_KEY_HEX: &str =
    "75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd";

/// Check if the rollup is available for testing
///
/// Returns true if the rollup is healthy, false otherwise.
/// Tests should skip gracefully if this returns false.
pub async fn is_rollup_available() -> bool {
    let rpc_url =
        std::env::var("ROLLUP_RPC_URL").unwrap_or_else(|_| "http://localhost:12346".to_string());
    let verifier_url =
        std::env::var("VERIFIER_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

    match Provider::new(&rpc_url, &verifier_url, &indexer_url).await {
        Ok(provider) => provider.is_healthy().await,
        Err(_) => false,
    }
}
