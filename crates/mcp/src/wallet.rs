use std::path::PathBuf;

use anyhow::{Context, Result};
use sov_cli::wallet_state::{AddressEntry, WalletState};
use sov_modules_api::transaction::{Transaction, UnsignedTransaction};
use sov_modules_api::{CredentialId, CryptoSpec, DispatchCall, PrivateKey, PublicKey, Spec};
use sov_modules_stf_blueprint::Runtime as RuntimeTrait;

/// Wallet context that manages wallet state and key operations
///
/// Simplified wallet that only requires a private key hex string.
/// All other data (public key, address) is automatically derived.
pub struct WalletContext<Tx, S>
where
    Tx: DispatchCall,
    S: Spec,
{
    /// Private key for signing transactions
    private_key: <S::CryptoSpec as CryptoSpec>::PrivateKey,
    /// Public key derived from the private key
    public_key: <S::CryptoSpec as CryptoSpec>::PublicKey,
    /// Address derived from the private key
    #[allow(dead_code)]
    address: S::Address,
    /// Legacy wallet state (optional, for backward compatibility)
    #[allow(dead_code)]
    wallet_state: Option<WalletState<Tx, S>>,
    /// Legacy wallet path (optional, for backward compatibility)
    #[allow(dead_code)]
    wallet_path: Option<PathBuf>,
}

impl<Tx, S> WalletContext<Tx, S>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    /// Create wallet from a private key hex string
    ///
    /// This is the primary way to create a wallet - just provide a private key as hex string
    /// and everything else (public key, address) is automatically derived.
    ///
    /// No files required! Just pass the private key and you're ready to sign transactions.
    ///
    /// # Parameters
    /// * `private_key_hex` - Private key as hex string (with or without "0x" prefix)
    ///
    /// # Example
    /// ```ignore
    /// let wallet = WalletContext::from_private_key_hex("your_private_key_hex_here")?;
    /// // That's it! Now you can sign transactions
    /// ```
    pub fn from_private_key_hex(private_key_hex: impl AsRef<str>) -> Result<Self> {
        let hex_str = private_key_hex.as_ref().trim();
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);

        // Decode hex to bytes
        let private_key_bytes =
            hex::decode(hex_str).context("Failed to decode private key hex string")?;

        // Create the JSON structure that matches the key file format
        // This works with both Ed25519 and Ethereum key types
        let key_json = serde_json::json!({
            "private_key": {
                "key_pair": private_key_bytes
            }
        });

        // First deserialize just the private_key part
        let private_key: <S::CryptoSpec as CryptoSpec>::PrivateKey =
            serde_json::from_value(key_json["private_key"].clone())
                .context("Failed to deserialize private key from JSON structure")?;

        let public_key = private_key.pub_key();
        let credential_id: CredentialId = public_key.credential_id();
        let address: S::Address = credential_id.into();

        tracing::info!("Wallet initialized from private key");
        tracing::info!("Address: {}", address);

        Ok(Self {
            private_key,
            public_key,
            address,
            wallet_state: None,
            wallet_path: None,
        })
    }

    /// Get the wallet address
    pub fn get_address(&self) -> S::Address {
        self.address.clone()
    }

    /// Get the public key
    pub fn default_public_key(&self) -> Result<<S::CryptoSpec as CryptoSpec>::PublicKey> {
        Ok(self.public_key.clone())
    }

    /// Get the private key (used internally for signing)
    pub fn load_default_private_key(&self) -> Result<<S::CryptoSpec as CryptoSpec>::PrivateKey> {
        Ok(self.private_key.clone())
    }

    /// Get the default address entry (legacy - for backward compatibility)
    #[deprecated(since = "0.3.0", note = "Use get_address() instead")]
    #[allow(dead_code)]
    pub fn default_address(&self) -> Option<&AddressEntry<S>> {
        self.wallet_state.as_ref()?.addresses.default_address()
    }

    /// Get the wallet path (legacy - for backward compatibility)
    #[deprecated(since = "0.3.0", note = "Wallet no longer uses file paths")]
    #[allow(dead_code)]
    pub fn wallet_path(&self) -> Option<&PathBuf> {
        self.wallet_path.as_ref()
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
        let signed_tx =
            Transaction::<Runtime, S>::new_signed_tx(&private_key, chain_hash, unsigned_tx);

        // Borsh-serialize the signed transaction
        let raw_tx =
            borsh::to_vec(&signed_tx).context("Failed to borsh-serialize signed transaction")?;

        tracing::debug!(
            "Transaction signed successfully, serialized size: {} bytes",
            raw_tx.len()
        );

        Ok(raw_tx)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use demo_stf::runtime::Runtime;
    use sov_address::MultiAddressEvm;
    use sov_ligero_adapter::Ligero;
    use sov_mock_da::MockDaSpec;
    use sov_mock_zkvm::MockZkvm;
    use sov_modules_api::capabilities::UniquenessData;
    use sov_modules_api::configurable_spec::ConfigurableSpec;
    use sov_modules_api::execution_mode::Native;
    use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
    use sov_modules_api::Amount;

    use super::*;
    use crate::test_utils::TEST_PRIVATE_KEY_HEX;

    // Define test spec types
    type TestSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
    type TestRuntime = Runtime<TestSpec>;

    #[test]
    fn test_load_wallet() {
        let result =
            WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX);

        assert!(
            result.is_ok(),
            "Failed to create wallet: {:?}",
            result.err()
        );

        let wallet = result.unwrap();

        // Verify wallet address matches expected test address
        let address = wallet.get_address();
        assert_eq!(
            address.to_string(),
            "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf",
            "Address should match test wallet address"
        );
    }

    #[test]
    fn test_default_address() {
        let wallet =
            WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

        let address = wallet.get_address();

        // Verify address format (should start with "sov1")
        let address_str = address.to_string();
        assert!(
            address_str.starts_with("sov1"),
            "Address should start with sov1, got: {}",
            address_str
        );

        // Verify address is the expected test address
        assert_eq!(
            address_str, "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf",
            "Address should match test wallet address"
        );
    }

    #[test]
    fn test_default_public_key() {
        let wallet =
            WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

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
        let wallet =
            WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

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
        let wallet =
            WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet");

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

        let proof_package_bytes =
            bincode::serialize(&proof_package).expect("Failed to serialize dummy proof package");

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

        let runtime_call =
            demo_stf::runtime::RuntimeCall::<TestSpec>::ValueSetterZk(value_setter_call);

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
        assert!(!raw_tx.is_empty(), "Signed transaction should not be empty");

        // Verify the transaction is at least a reasonable size (has signature + payload)
        assert!(
            raw_tx.len() > 64,
            "Signed transaction should be at least 64 bytes, got: {} bytes",
            raw_tx.len()
        );
    }

    #[test]
    fn test_wallet_from_hex() {
        // Test the new simplified method
        let wallet =
            WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)
                .expect("Failed to create wallet from hex");

        // Verify address
        let address = wallet.get_address();
        assert_eq!(
            address.to_string(),
            "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf",
            "Address should match test wallet address"
        );

        // Test with 0x prefix
        let wallet_with_prefix = WalletContext::<TestRuntime, TestSpec>::from_private_key_hex(
            &format!("0x{}", TEST_PRIVATE_KEY_HEX),
        )
        .expect("Failed to create wallet from hex with 0x prefix");

        let address_with_prefix = wallet_with_prefix.get_address();
        assert_eq!(
            address.to_string(),
            address_with_prefix.to_string(),
            "Addresses should match regardless of 0x prefix"
        );
    }
}
