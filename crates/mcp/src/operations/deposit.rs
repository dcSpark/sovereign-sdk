//! Deposit operation for Midnight Privacy module
//!
//! Provides functionality for creating deposit transactions that move funds from
//! transparent balance into the shielded pool.

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use midnight_privacy::CallMessage as MidnightCallMessage;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;

use crate::provider::Provider;
use crate::wallet::WalletContext;

// Use the same spec types as the MCP server
pub type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

/// Result of a deposit operation
#[derive(Debug)]
pub struct DepositResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
    /// Random nonce used for the note (for user records)
    pub rho: [u8; 32],
    /// Recipient binding used for the note (for user records)
    pub recipient: [u8; 32],
}

/// Create an unsigned transaction for deposit into the privacy pool
///
/// Encapsulates transaction creation logic including chain ID retrieval,
/// nonce/generation retrieval, and proper fee configuration.
pub async fn create_deposit_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    amount: u128,
    rho: [u8; 32],
    recipient: [u8; 32],
) -> Result<UnsignedTransaction<McpRuntime, McpSpec>> {
    let chain_data = provider
        .get_chain_data()
        .await
        .context("Failed to fetch chain data from rollup")?;

    let chain_id = chain_data.chain_id;

    tracing::info!("Using chain_id: {} ({})", chain_id, chain_data.chain_name);

    let deposit_call = MidnightCallMessage::<McpSpec>::Deposit {
        amount,
        rho,
        recipient,
        gas: None,
        view_fvks: None,
    };

    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::MidnightPrivacy(deposit_call);

    tracing::info!("Created runtime call for midnight_privacy deposit module");

    let public_key = wallet
        .default_public_key()
        .context("Failed to get public key from wallet")?;

    let nonce = provider
        .get_nonce::<McpSpec>(&public_key)
        .await
        .context("Failed to get nonce from provider")?;

    tracing::info!("Got nonce from rollup: {}", nonce);

    // Use timestamp as generation if nonce is 0
    let generation = if nonce == 0 {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        tracing::info!("Nonce is 0, using timestamp as generation: {}", timestamp);
        timestamp
    } else {
        tracing::info!("Using nonce as generation: {}", nonce);
        nonce
    };

    let max_fee = Amount::from(1_000_000_000_000u128);

    let unsigned_tx = UnsignedTransaction::<McpRuntime, McpSpec>::new(
        runtime_call,
        chain_id,
        PriorityFeeBips::ZERO,
        max_fee,
        UniquenessData::Generation(generation),
        None,
    );

    tracing::debug!("Created unsigned transaction");

    Ok(unsigned_tx)
}

/// Deposit funds into the Midnight Privacy shielded pool
///
/// This operation:
/// 1. Generates random rho and recipient values for the note
/// 2. Creates a deposit transaction that moves funds from transparent to shielded
/// 3. Signs and submits the transaction to the verifier service (which forwards to sequencer)
///
/// # Parameters
/// * `provider` - Provider for rollup connection
/// * `wallet` - Wallet context for signing
/// * `amount` - Amount to deposit (in the smallest unit)
///
/// # Returns
/// DepositResult containing the transaction hash and note parameters (rho, recipient)
pub async fn deposit(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    amount: u128,
) -> Result<DepositResult> {
    tracing::info!("Creating deposit for amount: {}", amount);

    // Generate random note parameters using rand::random() which is Send-safe
    let rho: [u8; 32] = rand::random();
    let recipient: [u8; 32] = rand::random();

    tracing::debug!(
        "Generated note parameters - rho: {}, recipient: {}",
        hex::encode(&rho),
        hex::encode(&recipient)
    );

    let unsigned_tx = create_deposit_unsigned_tx(provider, wallet, amount, rho, recipient).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    tracing::info!("Transaction signed and serialized: {} bytes", raw_tx.len());

    let tx_hash = provider
        .submit_to_verifier(raw_tx)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to verifier service")?;

    tracing::info!("Deposit transaction submitted successfully via verifier: {}", tx_hash);

    Ok(DepositResult {
        tx_hash,
        rho,
        recipient,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TEST_PRIVATE_KEY_HEX;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_deposit() {
        use crate::test_utils::is_rollup_available;

        if !is_rollup_available().await {
            eprintln!("⚠️  Skipping test: Rollup is not available at ROLLUP_RPC_URL");
            eprintln!("   Start the rollup or set ROLLUP_RPC_URL to run this test");
            return;
        }

        let amount = 100u128;

        tracing::info!("Creating wallet from private key");
        let wallet =
            WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

        let rpc_url = std::env::var("ROLLUP_RPC_URL")
            .unwrap_or_else(|_| "http://localhost:12346".to_string());
        let verifier_url = std::env::var("VERIFIER_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        let indexer_url = std::env::var("INDEXER_URL")
            .unwrap_or_else(|_| "http://localhost:13100".to_string());

        tracing::info!("Connecting to rollup at: {}", rpc_url);
        let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url)
            .await
            .expect("Failed to connect to rollup");

        tracing::info!("Calling deposit with amount: {}", amount);

        let result = deposit(&provider, &wallet, amount).await;

        assert!(result.is_ok(), "deposit should succeed: {:?}", result.err());

        let deposit_result = result.unwrap();

        assert!(
            !deposit_result.tx_hash.is_empty(),
            "tx_hash should not be empty"
        );
        assert_ne!(deposit_result.rho, [0u8; 32], "rho should be random");
        assert_ne!(
            deposit_result.recipient, [0u8; 32],
            "recipient should be random"
        );

        tracing::info!("✅ Deposit transaction submitted successfully!");
        tracing::info!("   Transaction hash: {}", deposit_result.tx_hash);
        tracing::info!("   Rho: {}", hex::encode(&deposit_result.rho));
        tracing::info!("   Recipient: {}", hex::encode(&deposit_result.recipient));
    }
}
