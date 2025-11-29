//! Update value with zero-knowledge proof generation
//!
//! Provides functionality for creating value-setter-zk transactions with Ligero zero-knowledge proofs.

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use serde::{Deserialize, Serialize};
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;

use crate::ligero::{Ligero, LigeroProgramArguments};
use crate::provider::Provider;
use crate::wallet::WalletContext;

// Use the same spec types as the MCP server
pub type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

/// Public output from the Ligero guest program (matches value-setter-zk module)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueProofPublic {
    /// The value that was proven to be valid
    pub value: u32,
}

/// Ligero proof package containing both the proof and serialized public output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LigeroProofPackage {
    /// The compressed Ligero proof bytes (from proof_data.gz)
    pub proof: Vec<u8>,
    /// Bincode-serialized public output committed by the guest program
    pub public_output: Vec<u8>,
}

/// Result of generating an update value proof with transaction
#[derive(Debug)]
pub struct UpdateValueZkResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

/// Build a Ligero proof package from raw proof bytes and value
///
/// This creates the proof package structure expected by the value-setter-zk module,
/// which includes both the compressed proof and the bincode-serialized public output.
///
/// # Parameters
/// * `proof_bytes` - The compressed Ligero proof bytes
/// * `value` - The value that was proven
///
/// # Returns
/// Bincode-serialized proof package ready for transaction
fn build_proof_package(proof_bytes: Vec<u8>, value: u32) -> Result<Vec<u8>> {
    let public_output = ValueProofPublic { value };
    let public_output_bytes =
        bincode::serialize(&public_output).context("Failed to bincode-serialize public output")?;

    tracing::debug!(
        "Serialized public output: {} bytes",
        public_output_bytes.len()
    );

    let proof_package = LigeroProofPackage {
        proof: proof_bytes,
        public_output: public_output_bytes,
    };

    let proof_package_bytes =
        bincode::serialize(&proof_package).context("Failed to bincode-serialize proof package")?;

    tracing::info!(
        "Created proof package: {} bytes total",
        proof_package_bytes.len()
    );

    Ok(proof_package_bytes)
}

/// Create an unsigned transaction for value-setter-zk with proof
///
/// Encapsulates transaction creation logic including chain ID retrieval,
/// nonce/generation retrieval, and proper fee configuration for ZK proof transactions.
pub async fn create_value_setter_zk_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    proof_package_bytes: Vec<u8>,
    value: u32,
) -> Result<UnsignedTransaction<McpRuntime, McpSpec>> {
    let chain_data = provider
        .get_chain_data()
        .await
        .context("Failed to fetch chain data from rollup")?;

    let chain_id = chain_data.chain_id;

    tracing::info!("Using chain_id: {} ({})", chain_id, chain_data.chain_name);

    let safe_proof = proof_package_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large for SafeVec"))?;

    let value_setter_call = sov_value_setter_zk::CallMessage::<McpSpec>::SetValueWithProof {
        value,
        proof: safe_proof,
        gas: None,
    };

    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::ValueSetterZk(value_setter_call);

    tracing::info!("Created runtime call for value_setter_zk module");

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

/// Generate a zero-knowledge proof for updating a value, sign and submit the transaction
///
/// Handles proof generation, transaction creation, signing, and submission to the rollup sequencer.
pub async fn update_value_zk(
    ligero: &Ligero,
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    value: i64,
) -> Result<UpdateValueZkResult> {
    let value_u32: u32 = value
        .try_into()
        .context("Value must be between 0 and 4,294,967,295 (u32::MAX)")?;

    tracing::info!("Generating proof for value: {}", value_u32);

    let proof_bytes = ligero
        .generate_proof(
            8192,
            8000,
            vec![1],
            vec![
                LigeroProgramArguments::I64 { i64: value },
                LigeroProgramArguments::I64 { i64: value },
            ],
        )
        .context("Failed to generate Ligero proof for value update")?;

    tracing::info!("Generated proof, size: {} bytes", proof_bytes.len());

    let proof_package_bytes = build_proof_package(proof_bytes, value_u32)?;

    let unsigned_tx =
        create_value_setter_zk_unsigned_tx(provider, wallet, proof_package_bytes, value_u32)
            .await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    tracing::info!("Transaction signed and serialized: {} bytes", raw_tx.len());

    let tx_hash = provider
        .submit_transaction(raw_tx)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to rollup")?;

    tracing::info!("Transaction submitted successfully: {}", tx_hash);

    Ok(UpdateValueZkResult { tx_hash })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::test_utils::{ligero::create_test_ligero, TEST_PRIVATE_KEY_HEX};

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_update_value_zk() {
        use crate::test_utils::is_rollup_available;

        if !is_rollup_available().await {
            eprintln!("⚠️  Skipping test: Rollup is not available at ROLLUP_RPC_URL");
            eprintln!("   Start the rollup or set ROLLUP_RPC_URL to run this test");
            return;
        }

        let ligero = create_test_ligero();
        let value = 60000i64;

        tracing::info!("Creating wallet from private key");
        let wallet =
            WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

        let rpc_url = std::env::var("ROLLUP_RPC_URL")
            .unwrap_or_else(|_| "http://localhost:12346".to_string());

        tracing::info!("Connecting to rollup at: {}", rpc_url);
        let provider = Provider::new(&rpc_url)
            .await
            .expect("Failed to connect to rollup");

        tracing::info!("Calling update_value_zk with value: {}", value);

        let result = update_value_zk(&ligero, &provider, &wallet, value).await;

        assert!(
            result.is_ok(),
            "update_value_zk should succeed: {:?}",
            result.err()
        );

        let update_result = result.unwrap();

        assert!(
            !update_result.tx_hash.is_empty(),
            "tx_hash should not be empty"
        );

        tracing::info!("✅ Transaction submitted successfully!");
        tracing::info!("   Transaction hash: {}", update_result.tx_hash);
    }
}
