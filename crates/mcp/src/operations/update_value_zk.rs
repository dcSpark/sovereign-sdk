//! Update value with zero-knowledge proof generation

use anyhow::{Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use demo_stf::runtime::Runtime;
use serde::{Deserialize, Serialize};
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::Amount;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, Transaction, UnsignedTransaction};
use sov_modules_stf_blueprint::Runtime as RuntimeTrait;

use crate::ligero::{Ligero, LigeroProgramArguments};
use crate::wallet::WalletContext;

// Use the same spec types as the MCP server
type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;

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

/// Transaction message for value-setter-zk module
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub enum ValueSetterZkCallMessage {
    #[borsh(rename = "set_value_with_proof")]
    SetValueWithProof {
        /// The value to set
        value: u32,
        /// Bincode-serialized LigeroProofPackage
        proof: Vec<u8>,
        /// Gas to charge (None for no gas)
        gas: Option<()>, // Simplified - actual type depends on Spec
    },
}

/// Result of generating an update value proof with transaction
#[derive(Debug)]
pub struct UpdateValueZkResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

/// Generate a zero-knowledge proof for updating a value, sign and submit the transaction
///
/// This function encapsulates the complete business logic for:
/// 1. Generating a ZK proof for value updates
/// 2. Building the transaction call message with the proof
/// 3. Creating and signing the transaction
/// 4. Submitting to the rollup sequencer
///
/// The proof package is created using bincode serialization (not JSON) and contains:
/// - The actual proof bytes (compressed from Ligero)
/// - The public output (value) bincode-serialized
///
/// # Parameters
/// * `ligero` - The Ligero proof generator
/// * `wallet` - The wallet context for signing and submitting
/// * `value` - The value to set (must fit in u32 for value-setter-zk module)
/// * `chain_id` - The chain ID for the transaction
///
/// # Returns
/// A result containing the transaction hash from the rollup
pub async fn update_value_zk(
    ligero: &Ligero,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    value: i64,
    chain_id: u64,
) -> Result<UpdateValueZkResult> {
    // Validate value fits in u32
    let value_u32: u32 = value
        .try_into()
        .context("Value must be between 0 and 4,294,967,295 (u32::MAX)")?;

    tracing::info!("Generating proof for value: {}", value_u32);

    // Generate proof with Ligero
    // The second argument (old_value) is private (index 1)
    let proof_bytes = ligero
        .generate_proof(
            8192,    // packing
            8000,    // gpu_threads
            vec![1], // private_indices - old_value is private
            vec![
                LigeroProgramArguments::I64 { i64: value },
                LigeroProgramArguments::I64 { i64: value },
            ],
        )
        .context("Failed to generate Ligero proof for value update")?;

    let proof_size = proof_bytes.len();
    tracing::info!("Generated proof, size: {} bytes", proof_size);

    // Step 1: Create the public output and serialize it with bincode
    let public_output = ValueProofPublic { value: value_u32 };
    let public_output_bytes =
        bincode::serialize(&public_output).context("Failed to bincode-serialize public output")?;

    tracing::debug!(
        "Serialized public output: {} bytes",
        public_output_bytes.len()
    );

    // Step 2: Create the Ligero proof package with bincode serialization
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

    // Step 3: Convert proof to SafeVec and create the runtime call
    let safe_proof = proof_package_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large for SafeVec"))?;

    let value_setter_call = sov_value_setter_zk::CallMessage::<McpSpec>::SetValueWithProof {
        value: value_u32,
        proof: safe_proof,
        gas: None,
    };

    // Step 4: Wrap in runtime call
    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::ValueSetterZk(value_setter_call);

    tracing::info!("Created runtime call for value_setter_zk module");

    // Step 5: Get nonce for transaction signing
    let nonce = wallet
        .get_default_nonce()
        .await
        .context("Failed to get nonce from wallet")?;

    tracing::info!("Got nonce from rollup: {}", nonce);

    // If nonce is 0, use current timestamp (the uniqueness system uses timestamps in ms)
    let generation = if nonce == 0 {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        tracing::info!("Nonce is 0, using current timestamp as generation: {}", timestamp);
        timestamp
    } else {
        tracing::info!("Using nonce as generation: {}", nonce);
        nonce
    };

    // Step 6: Create unsigned transaction with the runtime call
    // Set a high max_fee since ZK proof transactions are expensive (large proof size)
    let max_fee = Amount::from(1_000_000_000_000u128); // 1 trillion units - enough for large ZK proofs

    let unsigned_tx = UnsignedTransaction::<McpRuntime, McpSpec>::new(
        runtime_call,
        chain_id,
        PriorityFeeBips::ZERO,
        max_fee,
        UniquenessData::Generation(generation),
        None, // gas_limit set to None - let the system calculate
    );

    tracing::debug!("Created unsigned transaction");

    // Step 7: Load private key and sign the transaction
    let private_key = wallet
        .load_default_private_key()
        .context("Failed to load private key from wallet")?;

    let chain_hash = &McpRuntime::CHAIN_HASH;
    let signed_tx =
        Transaction::<McpRuntime, McpSpec>::new_signed_tx(&private_key, chain_hash, unsigned_tx);

    tracing::debug!("Transaction signed successfully");

    // Step 8: Borsh-serialize the signed transaction
    let raw_tx =
        borsh::to_vec(&signed_tx).inspect_err(|e| tracing::info!("Failed to Borsh-serialize signed transaction: {:?}", e)).context("Failed to Borsh-serialize signed transaction")?;

    let transaction_size = raw_tx.len();
    tracing::info!("Serialized signed transaction: {} bytes", transaction_size);

    // Step 9: Submit transaction to rollup
    let tx_hash = wallet
        .submit_transaction(raw_tx.clone())
        .await
        .inspect_err(|e| tracing::info!("Failed to submit transaction to rollup: {:?}", e))
        .context("Failed to submit transaction to rollup")?;

    tracing::info!("Transaction submitted successfully: {}", tx_hash);

    Ok(UpdateValueZkResult { tx_hash })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Helper function to get the platform-specific binary directory
    fn get_platform_bin_dir() -> &'static str {
        if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux-amd64"
        } else {
            panic!("Unsupported platform for Ligero tests");
        }
    }

    /// Helper function to create a Ligero instance for testing
    fn create_test_ligero() -> Ligero {
        let platform_dir = get_platform_bin_dir();

        Ligero::new(
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!(
                        "../adapters/ligero/bins/{}/bin/webgpu_prover",
                        platform_dir
                    ))
                    .canonicalize()
                    .expect("Failed to find prover binary"),
            ),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!(
                        "../adapters/ligero/bins/{}/bin/webgpu_verifier",
                        platform_dir
                    ))
                    .canonicalize()
                    .expect("Failed to find verifier binary"),
            ),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("../adapters/ligero/bins/{}/shader", platform_dir))
                    .canonicalize()
                    .expect("Failed to find shader directory"),
            ),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../adapters/ligero/bins/programs/value_validator.wasm")
                    .canonicalize()
                    .expect("Failed to find value_validator.wasm"),
            ),
        )
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    // #[ignore] // Run with: cargo test -p mcp test_update_value_zk -- --ignored
    async fn test_update_value_zk() {
        // This test validates the complete update_value_zk flow
        // To run this test, you need:
        // 1. A wallet at ~/.sovereign/wallet.json
        // 2. A running rollup node at http://localhost:12345
        // Run with: cargo test -p mcp test_update_value_zk -- --ignored

        let ligero = create_test_ligero();
        let value = 60000i64;
        let chain_id = 4321;

        // Try to load wallet - will fail if not available
        let wallet_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("test-data/wallet_state.json",))
            .canonicalize().unwrap().display().to_string();

        let rpc_url = std::env::var("ROLLUP_RPC_URL")
            .unwrap_or_else(|_| "http://localhost:12346".to_string());

        tracing::info!("Loading wallet from: {}", wallet_path);
        tracing::info!("Connecting to rollup at: {}", rpc_url);

        let wallet = WalletContext::<McpRuntime, McpSpec>::load(&wallet_path, Some(&rpc_url))
            .await
            .expect("Failed to load wallet - make sure wallet exists and RPC is running");

        tracing::info!("Calling update_value_zk with value: {}", value);

        // Call the actual function
        let result = update_value_zk(&ligero, &wallet, value, chain_id).await;

        // Validate the result
        assert!(
            result.is_ok(),
            "update_value_zk should succeed: {:?}",
            result.err()
        );

        let update_result = result.unwrap();

        // Assert we get a transaction hash
        assert!(
            !update_result.tx_hash.is_empty(),
            "tx_hash should not be empty"
        );

        tracing::info!("✅ Transaction submitted successfully!");
        tracing::info!("   Transaction hash: {}", update_result.tx_hash);
    }
}
