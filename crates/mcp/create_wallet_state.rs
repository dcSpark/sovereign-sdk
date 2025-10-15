// Helper script to create a wallet state JSON from an existing private key file
// Usage: cargo run --bin create_wallet_state

use demo_stf::genesis_config::create_demo_rollup_genesis;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::{CredentialId, PrivateKey, PublicKey};
use std::path::PathBuf;

type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;

fn main() -> anyhow::Result<()> {
    // Load the existing private key file
    let private_key_path = PathBuf::from("/Users/agallardol/Documents/github/dcpsark-sovereign-sdk/examples/test-data/keys/token_deployer_private_key.json");

    // Read and parse the private key
    let key_data: serde_json::Value = serde_json::from_slice(&std::fs::read(&private_key_path)?)?;
    let private_key_bytes = key_data["private_key"]["key_pair"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid private key format"))?
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect::<Vec<u8>>();

    // Deserialize the private key
    let private_key: <McpSpec as sov_modules_api::Spec>::CryptoSpec::PrivateKey =
        borsh::from_slice(&private_key_bytes)?;

    // Compute public key and address
    let pub_key = private_key.pub_key();
    let credential_id: CredentialId = pub_key.credential_id();
    let address: <McpSpec as sov_modules_api::Spec>::Address = credential_id.into();

    // Serialize public key as hex (matching pubkey_serde format)
    let pub_key_bytes = borsh::to_vec(&pub_key)?;
    let pub_key_hex = format!("0x{}", hex::encode(&pub_key_bytes));

    // Create the wallet state JSON
    let wallet_state = serde_json::json!({
        "version": "0.3.0",
        "unsent_transactions": [],
        "addresses": {
            "addresses": [
                {
                    "address": address.to_string(),
                    "nickname": null,
                    "location": private_key_path.to_str().unwrap(),
                    "pub_key": pub_key_hex
                }
            ]
        },
        "rest_api_url": null
    });

    // Write the wallet state
    let output_path = PathBuf::from("/Users/agallardol/Documents/github/dcpsark-sovereign-sdk/examples/test-data/keys/wallet_state.json");
    std::fs::write(&output_path, serde_json::to_string_pretty(&wallet_state)?)?;

    println!("Wallet state created at: {}", output_path.display());
    println!("Address: {}", address);
    println!("Public key: {}", pub_key_hex);

    Ok(())
}
