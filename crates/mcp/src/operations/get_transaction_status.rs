//! Get transaction status
//!
//! This module provides functionality for retrieving the current status of a specific transaction.

use crate::provider::{Provider, TransactionStatus};
use anyhow::{Context, Result};

/// Get the status of a transaction by its ID
///
/// Retrieves the current status of a specific transaction from the sequencer.
/// Returns the transaction status (e.g., "pending", "confirmed", "failed").
///
/// # Parameters
/// * `provider` - The RPC provider for sequencer queries
/// * `tx_hash` - The transaction hash ID (with or without 0x prefix)
///
/// # Returns
/// Transaction status containing the ID and status string
///
/// # Example
/// ```rust,no_run
/// # async fn example(provider: &mcp::provider::Provider) -> anyhow::Result<()> {
/// use mcp::operations::get_transaction_status;
///
/// let status = get_transaction_status(provider, "0x1234...").await?;
/// println!("Transaction {} status: {}", status.id, status.status);
/// # Ok(())
/// # }
/// ```
pub async fn get_transaction_status(
    provider: &Provider,
    tx_hash: &str,
) -> Result<TransactionStatus> {
    tracing::info!("Getting transaction status: {}", tx_hash);

    let tx_status = provider
        .get_transaction_status(tx_hash)
        .await
        .with_context(|| format!("Failed to get transaction status for {}", tx_hash))?;

    tracing::info!("Transaction {} status: {}", tx_status.id, tx_status.status);

    Ok(tx_status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_get_transaction_status_with_update_value_zk() {
        use crate::operations::update_value_zk;
        use crate::test_utils::{
            is_rollup_available, ligero::create_test_ligero, TEST_PRIVATE_KEY_HEX,
        };
        use crate::wallet::WalletContext;
        use demo_stf::runtime::Runtime;
        use sov_address::MultiAddressEvm;
        use sov_ligero_adapter::Ligero as LigeroAdapter;
        use sov_mock_da::MockDaSpec;
        use sov_mock_zkvm::MockZkvm;
        use sov_modules_api::configurable_spec::ConfigurableSpec;
        use sov_modules_api::execution_mode::Native;

        if !is_rollup_available().await {
            eprintln!("⚠️  Skipping test: Rollup is not available at ROLLUP_RPC_URL");
            eprintln!("   Start the rollup or set ROLLUP_RPC_URL to run this test");
            return;
        }

        type McpSpec =
            ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
        type McpRuntime = Runtime<McpSpec>;

        let ligero = create_test_ligero();
        let value = 60000i64;

        tracing::info!("Creating wallet from private key");
        let wallet =
            WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

        let rpc_url = std::env::var("ROLLUP_RPC_URL")
            .unwrap_or_else(|_| "http://localhost:12346".to_string());
        let verifier_url = std::env::var("VERIFIER_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());

        tracing::info!("Connecting to rollup at: {}", rpc_url);
        let provider = Provider::new(&rpc_url, &verifier_url)
            .await
            .expect("Failed to connect to rollup");

        tracing::info!(
            "Submitting transaction with update_value_zk (value: {})",
            value
        );
        let update_result = update_value_zk(&ligero, &provider, &wallet, value)
            .await
            .expect("Failed to submit transaction");

        let tx_hash = update_result.tx_hash;
        tracing::info!("✅ Transaction submitted: {}", tx_hash);

        tracing::info!("Getting transaction status for: {}", tx_hash);
        let status_result = get_transaction_status(&provider, &tx_hash)
            .await
            .expect("Failed to get transaction status");

        tracing::info!("✅ Transaction status: {}", status_result.status);

        assert_eq!(
            status_result.id.to_lowercase(),
            tx_hash.to_lowercase(),
            "Transaction ID should match the submitted hash"
        );
        assert!(
            !status_result.status.is_empty(),
            "Transaction status should not be empty"
        );

        tracing::info!("✅ Test completed successfully!");
    }
}
