//! Utility binary to generate 1000 deterministic keypairs and create a genesis bank.json file

use anyhow::Result;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::{PrivateKey, PublicKey};
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_ligero::MockDemoRollup;
use std::path::PathBuf;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GasTokenConfig {
    token_name: String,
    token_decimals: u8,
    address_and_balances: Vec<(String, String)>,
    admins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BankGenesis {
    gas_token_config: GasTokenConfig,
    tokens: Vec<serde_json::Value>,
}

/// Generate a deterministic keypair from an index
fn generate_deterministic_keypair(index: usize) -> Result<PrivateKeyAndAddress<DemoRollupSpec>> {
    const BASE_SEED: [u8; 32] = [
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42,
    ];

    // Derive a unique seed for each account by combining base seed with index
    let mut account_seed = BASE_SEED;
    let index_bytes = (index as u64).to_le_bytes();
    // XOR the last 8 bytes with the index to ensure uniqueness
    for (j, &byte) in index_bytes.iter().enumerate() {
        account_seed[24 + j] ^= byte;
    }

    // Create deterministic RNG from the seed and generate the signing key
    let mut rng = StdRng::from_seed(account_seed);
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rng);

    // Serialize/deserialize to convert to the correct private key type
    type PrivKey = <<DemoRollupSpec as sov_modules_api::Spec>::CryptoSpec as sov_rollup_interface::zk::CryptoSpec>::PrivateKey;
    let private_key: PrivKey = bincode::deserialize(&bincode::serialize(&signing_key)?)?;

    let pub_key = private_key.pub_key();
    let address: <DemoRollupSpec as sov_modules_api::Spec>::Address =
        pub_key.credential_id().into();

    Ok(PrivateKeyAndAddress {
        private_key,
        address,
    })
}

fn main() -> Result<()> {
    let num_accounts = 5000;
    let balance_per_account = "10000000000000"; // 10 trillion (enough for many transactions)

    println!("Generating {} deterministic accounts...", num_accounts);

    let mut address_and_balances = Vec::with_capacity(num_accounts + 8); // Extra space for system accounts
    let mut keypairs = Vec::with_capacity(num_accounts);

    // IMPORTANT: Add the sequencer account first (required for genesis initialization)
    // This account needs at least 2 trillion for the sequencer bond
    println!("Adding sequencer account (required for initialization)...");
    address_and_balances.push((
        "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf".to_string(),
        "5000000000000".to_string(), // 5 trillion (same as original genesis)
    ));

    // Add other important system accounts from original genesis
    println!("Adding paymaster account...");
    address_and_balances.push((
        "sov1x3jtvq0zwhj2ucsc4hqugskvralrulxvf53vwtkred93s85ar2a".to_string(),
        "5000000000000000".to_string(), // 5 quadrillion (paymaster needs more)
    ));

    // Add Ethereum accounts from original genesis (for EVM compatibility)
    println!("Adding EVM-compatible accounts...");
    address_and_balances.push((
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
        "5000000000000000".to_string(),
    ));
    address_and_balances.push((
        "0x3FE0233e6cf3c9753fcB7449987EC49C88aDDE71".to_string(),
        "5000000000000".to_string(),
    ));
    address_and_balances.push((
        "0x4Fa6c577eE74B4F3C5309Af1b6313dd6D525e694".to_string(),
        "5000000000000".to_string(),
    ));
    address_and_balances.push((
        "0xa80749aD39A047603cbc5D0f46a03A1c6B2Db6c9".to_string(),
        "5000000000000".to_string(),
    ));

    println!("Generating {} deterministic test accounts...", num_accounts);

    for i in 0..num_accounts {
        if i % 100 == 0 {
            println!("  Generated {}/{} accounts...", i, num_accounts);
        }

        let keypair = generate_deterministic_keypair(i)?;
        let address_str = format!("{}", keypair.address);
        address_and_balances.push((address_str, balance_per_account.to_string()));
        keypairs.push(keypair);
    }

    println!("Generated all {} test accounts!", num_accounts);

    // Use the sequencer account as admin (same as original genesis)
    let admin_address = "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf".to_string();

    // Save the total count before moving
    let total_accounts = address_and_balances.len();

    // Create genesis config
    let genesis = BankGenesis {
        gas_token_config: GasTokenConfig {
            token_name: "sov-token".to_string(),
            token_decimals: 6,
            address_and_balances,
            admins: vec![admin_address],
        },
        tokens: vec![],
    };

    // Write genesis file
    let genesis_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test-data/genesis/demo/mock");

    let bank_json_path = genesis_dir.join("bank.json");
    println!("\nWriting genesis file to: {}", bank_json_path.display());

    let json = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&bank_json_path, json)?;

    println!("✓ Genesis file written successfully!");

    // Also save all keypairs to a separate file for reference
    let keypairs_path = genesis_dir.join("generated_keypairs.json");
    println!("Writing keypairs to: {}", keypairs_path.display());

    let keypairs_json = serde_json::to_string_pretty(&keypairs)?;
    std::fs::write(&keypairs_path, keypairs_json)?;

    println!("✓ Keypairs file written successfully!");

    println!("\n========================================");
    println!("Summary:");
    println!("========================================");
    println!("Total accounts in genesis: {}", total_accounts);
    println!("  - System accounts: 6");
    println!("    * Sequencer (with 5T for bond)");
    println!("    * Paymaster (with 5Q)");
    println!("    * 4 EVM-compatible accounts");
    println!("  - Test accounts: {}", num_accounts);
    println!("\nFirst 5 test account addresses:");
    for i in 0..5.min(num_accounts) {
        println!("  Account {}: {}", i, keypairs[i].address);
    }

    Ok(())
}
