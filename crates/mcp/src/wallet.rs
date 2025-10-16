use anyhow::{Context, Result};
use sov_cli::wallet_state::{AddressEntry, WalletState};
use sov_cli::workflows::keys::load_key;
use sov_modules_api::{CryptoSpec, DispatchCall, Spec};
use sov_modules_api::transaction::{Transaction, UnsignedTransaction};
use sov_modules_stf_blueprint::Runtime as RuntimeTrait;
use std::path::{Path, PathBuf};

/// Wallet context that manages wallet state and key operations
///
/// Responsibilities:
/// - Load and manage wallet state (addresses, keys)
/// - Provide access to addresses and public keys
/// - Load private keys for signing
///
/// Does NOT handle:
/// - RPC communication (use Provider instead)
/// - Chain state queries (use Provider instead)
/// - Transaction submission (use Provider instead)
pub struct WalletContext<Tx, S>
where
    Tx: DispatchCall,
    S: Spec,
{
    wallet_state: WalletState<Tx, S>,
    #[allow(dead_code)]
    wallet_path: PathBuf,
}

impl<Tx, S> WalletContext<Tx, S>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    /// Load wallet from a path
    pub fn load(wallet_path: impl AsRef<Path>) -> Result<Self> {
        let wallet_path = wallet_path.as_ref().to_path_buf();
        let wallet_state = WalletState::<Tx, S>::load(&wallet_path)
            .with_context(|| format!("Failed to load wallet from {}", wallet_path.display()))?;

        Ok(Self {
            wallet_state,
            wallet_path,
        })
    }

    /// Get the default (active) address entry
    pub fn default_address(&self) -> Option<&AddressEntry<S>> {
        self.wallet_state.addresses.default_address()
    }

    /// Get the public key of the default address
    pub fn default_public_key(&self) -> Result<<S::CryptoSpec as CryptoSpec>::PublicKey> {
        let address_entry = self
            .default_address()
            .ok_or_else(|| anyhow::anyhow!("No default address in wallet"))?;
        Ok(address_entry.pub_key.clone())
    }

    /// Load the private key for the default address
    pub fn load_default_private_key(&self) -> Result<<S::CryptoSpec as CryptoSpec>::PrivateKey> {
        let address_entry = self
            .default_address()
            .ok_or_else(|| anyhow::anyhow!("No default address in wallet"))?;

        load_key::<S>(&address_entry.location).with_context(|| {
            format!(
                "Failed to load key from {}",
                address_entry.location.display()
            )
        })
    }

    /// Get the wallet path
    #[allow(dead_code)]
    pub fn wallet_path(&self) -> &PathBuf {
        &self.wallet_path
    }

    /// Sign a transaction using the default wallet key
    ///
    /// This method encapsulates all transaction signing logic:
    /// 1. Loads the private key for the default address
    /// 2. Signs the transaction with the chain hash
    /// 3. Borsh-serializes the signed transaction
    ///
    /// # Parameters
    /// * `unsigned_tx` - The unsigned transaction to sign
    ///
    /// # Returns
    /// The borsh-serialized signed transaction ready for submission
    ///
    /// # Type Parameters
    /// * `Runtime` - The runtime type that implements both DispatchCall and RuntimeTrait
    pub fn sign_transaction<Runtime>(
        &self,
        unsigned_tx: UnsignedTransaction<Runtime, S>,
    ) -> Result<Vec<u8>>
    where
        Runtime: DispatchCall + RuntimeTrait<S>,
        Tx: From<Runtime>,
    {
        // Load the private key for signing
        let private_key = self
            .load_default_private_key()
            .context("Failed to load private key for transaction signing")?;

        // Get the chain hash from the runtime
        let chain_hash = &Runtime::CHAIN_HASH;

        // Sign the transaction
        let signed_tx = Transaction::<Runtime, S>::new_signed_tx(&private_key, chain_hash, unsigned_tx);

        // Borsh-serialize the signed transaction
        let raw_tx = borsh::to_vec(&signed_tx)
            .context("Failed to borsh-serialize signed transaction")?;

        tracing::debug!(
            "Transaction signed successfully, serialized size: {} bytes",
            raw_tx.len()
        );

        Ok(raw_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use demo_stf::runtime::Runtime;
    use sov_address::MultiAddressEvm;
    use sov_ligero_adapter::Ligero;
    use sov_mock_da::MockDaSpec;
    use sov_mock_zkvm::MockZkvm;
    use sov_modules_api::configurable_spec::ConfigurableSpec;
    use sov_modules_api::execution_mode::Native;
    use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
    use sov_modules_api::{Amount, capabilities::UniquenessData};
    use std::path::PathBuf;

    // Define test spec types
    type TestSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
    type TestRuntime = Runtime<TestSpec>;

    fn get_test_wallet_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data/wallet_state.json")
    }

    #[test]
    fn test_load_wallet() {
        let wallet_path = get_test_wallet_path();

        let result = WalletContext::<TestRuntime, TestSpec>::load(&wallet_path);

        assert!(
            result.is_ok(),
            "Failed to load wallet: {:?}",
            result.err()
        );

        let wallet = result.unwrap();

        // Verify wallet has a default address
        assert!(
            wallet.default_address().is_some(),
            "Wallet should have a default address"
        );
    }

    #[test]
    fn test_default_address() {
        let wallet_path = get_test_wallet_path();
        let wallet = WalletContext::<TestRuntime, TestSpec>::load(&wallet_path)
            .expect("Failed to load wallet");

        let address_entry = wallet.default_address();

        assert!(address_entry.is_some(), "Wallet should have a default address");

        let address_entry = address_entry.unwrap();

        // Verify address format (should start with "sov1")
        let address_str = address_entry.address.to_string();
        assert!(
            address_str.starts_with("sov1"),
            "Address should start with sov1, got: {}",
            address_str
        );

        // Verify address is the expected test address
        assert_eq!(
            address_str,
            "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf",
            "Address should match test wallet address"
        );
    }

    #[test]
    fn test_default_public_key() {
        let wallet_path = get_test_wallet_path();
        let wallet = WalletContext::<TestRuntime, TestSpec>::load(&wallet_path)
            .expect("Failed to load wallet");

        let result = wallet.default_public_key();

        assert!(
            result.is_ok(),
            "Should be able to get default public key: {:?}",
            result.err()
        );

        // Successfully retrieved public key
        let _pub_key = result.unwrap();
    }

    #[test]
    fn test_load_default_private_key() {
        let wallet_path = get_test_wallet_path();
        let wallet = WalletContext::<TestRuntime, TestSpec>::load(&wallet_path)
            .expect("Failed to load wallet");

        let result = wallet.load_default_private_key();

        assert!(
            result.is_ok(),
            "Should be able to load default private key: {:?}",
            result.err()
        );

        // Successfully retrieved private key
        let _private_key = result.unwrap();
    }

    #[test]
    fn test_sign_transaction() {
        let wallet_path = get_test_wallet_path();
        let wallet = WalletContext::<TestRuntime, TestSpec>::load(&wallet_path)
            .expect("Failed to load wallet");

        // Create a dummy proof package for testing (we're only testing signing, not verification)
        // This mimics the structure from update_value_zk operation
        let dummy_proof = vec![0u8; 100]; // Dummy proof bytes
        let dummy_public_output = vec![0u8; 32]; // Dummy public output

        // Create a proof package structure (bincode serialized)
        #[derive(serde::Serialize)]
        struct DummyProofPackage {
            proof: Vec<u8>,
            public_output: Vec<u8>,
        }

        let proof_package = DummyProofPackage {
            proof: dummy_proof,
            public_output: dummy_public_output,
        };

        let proof_package_bytes = bincode::serialize(&proof_package)
            .expect("Failed to serialize dummy proof package");

        // Convert to SafeVec for the module
        let safe_proof = proof_package_bytes
            .try_into()
            .expect("Proof too large for SafeVec");

        // Create a ValueSetterZk transaction (reusing the existing dependency)
        let value_setter_call = sov_value_setter_zk::CallMessage::<TestSpec>::SetValueWithProof {
            value: 42,
            proof: safe_proof,
            gas: None,
        };

        let runtime_call = demo_stf::runtime::RuntimeCall::<TestSpec>::ValueSetterZk(value_setter_call);

        let unsigned_tx = UnsignedTransaction::<TestRuntime, TestSpec>::new(
            runtime_call,
            4321, // chain_id
            PriorityFeeBips::ZERO,
            Amount::from(1_000_000u128),
            UniquenessData::Generation(1234567890),
            None,
        );

        // Sign the transaction
        let result = wallet.sign_transaction::<TestRuntime>(unsigned_tx);

        assert!(
            result.is_ok(),
            "Should be able to sign transaction: {:?}",
            result.err()
        );

        let raw_tx = result.unwrap();

        // Verify the signed transaction is not empty
        assert!(
            !raw_tx.is_empty(),
            "Signed transaction should not be empty"
        );

        // Verify the transaction is at least a reasonable size (has signature + payload)
        assert!(
            raw_tx.len() > 64,
            "Signed transaction should be at least 64 bytes, got: {} bytes",
            raw_tx.len()
        );
    }

    #[test]
    fn test_wallet_path() {
        let wallet_path = get_test_wallet_path();
        let wallet = WalletContext::<TestRuntime, TestSpec>::load(&wallet_path)
            .expect("Failed to load wallet");

        let stored_path = wallet.wallet_path();

        assert_eq!(
            stored_path, &wallet_path,
            "Stored wallet path should match loaded path"
        );
    }
}

