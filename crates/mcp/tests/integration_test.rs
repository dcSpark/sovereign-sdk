//! Integration tests for the MCP server
//!
//! These tests require a running rollup node, sequencer, and verifier service.
//! Make sure all services are running before executing these tests.
//! Environment variables (WALLET_PRIVATE_KEY, ROLLUP_RPC_URL, VERIFIER_URL, PRIVPOOL_SPEND_KEY)
//! should be set in .env (INDEXER_URL is optional, defaults to http://localhost:13100).

use anyhow::Result;
use demo_stf::runtime::Runtime;
use mcp::ligero::Ligero;
use mcp::operations::{deposit, transfer};
use mcp::privacy_key::PrivacyKey;
use mcp::provider::Provider;
use mcp::wallet::WalletContext;
use sov_address::MultiAddressEvm;
use sov_bank::{config_gas_token_id, TokenId};
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;

type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;

/// Helper to check if services are available
async fn check_services_available(rpc_url: &str, verifier_url: &str, indexer_url: &str) -> bool {
    // Check rollup
    let provider_result = Provider::new(rpc_url, verifier_url, indexer_url).await;
    if provider_result.is_err() {
        eprintln!("⚠️  Rollup not available at {}", rpc_url);
        return false;
    }

    let _provider = provider_result.unwrap();

    // Check verifier
    let verifier_check = reqwest::get(format!("{}/health", verifier_url)).await;
    if verifier_check.is_err() {
        eprintln!("⚠️  Verifier service not available at {}", verifier_url);
        return false;
    }

    true
}

/// Helper to create test ligero prover
fn create_test_ligero() -> Ligero {
    let base_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("..")
        .join("..");

    let os_name = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        panic!("Unsupported OS for Ligero prover");
    };

    let prover_binary_path = base_path
        .join("crates/adapters/ligero/bins")
        .join(os_name)
        .join("bin/webgpu_prover");

    let shader_path = base_path
        .join("crates/adapters/ligero/bins")
        .join(os_name)
        .join("shader");

    let program_path =
        base_path.join("crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm");

    Ligero::new(
        Some(prover_binary_path),
        None,
        Some(shader_path),
        Some(program_path),
    )
}

#[tokio::test]
#[tracing_test::traced_test]
async fn test_deposit_and_transfer_flow() -> Result<()> {
    // Load .env file
    let _ = dotenvy::dotenv();

    // Initialize tracing for better debugging
    tracing::info!("Starting deposit and transfer integration test");

    // Get configuration from environment
    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");
    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());
    let privpool_spend_key = std::env::var("PRIVPOOL_SPEND_KEY")
        .expect("PRIVPOOL_SPEND_KEY must be set in .env (hex or privpool1... address)");

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running at ROLLUP_RPC_URL and VERIFIER_URL"
    );

    tracing::info!("Using ROLLUP_RPC_URL: {}", rpc_url);
    tracing::info!("Using VERIFIER_URL: {}", verifier_url);
    tracing::info!("Using INDEXER_URL: {}", indexer_url);

    // Parse privacy key from either raw spend key hex or bech32m address
    let privacy_key = if privpool_spend_key.starts_with("privpool1") {
        PrivacyKey::from_address(&privpool_spend_key)
    } else {
        PrivacyKey::from_hex(&privpool_spend_key)
    }
    .expect("Failed to parse PRIVPOOL_SPEND_KEY");
    tracing::info!("Using privacy address: {}", privacy_key.privacy_address());

    // Step 1: Create wallet from private key
    tracing::info!("Step 1: Creating wallet from private key");
    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let wallet_address = wallet.get_address();
    let wallet_address_str = wallet_address.to_string();
    tracing::info!("Wallet address: {}", wallet_address_str);

    // Step 1a: Verify wallet address format (should start with "sov1")
    assert!(
        wallet_address_str.starts_with("sov1"),
        "Wallet address should start with 'sov1', got: {}",
        wallet_address_str
    );
    tracing::info!("✓ Wallet address has correct format");

    // Step 2: Connect to provider
    tracing::info!("Step 2: Connecting to rollup and verifier");
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;
    tracing::info!("✓ Connected to services");

    // Step 3: Check wallet balance
    tracing::info!("Step 3: Checking wallet balance");
    let token_id: TokenId = config_gas_token_id();
    let balance = provider
        .get_balance::<McpSpec>(&wallet_address, &token_id)
        .await?;

    let balance_u128: u128 = balance.0;
    tracing::info!("Wallet balance: {}", balance_u128);

    // Step 3a: Verify balance is sufficient (> 2000000000000)
    assert!(
        balance_u128 > 2_000_000_000_000,
        "Wallet balance should be greater than 2000000000000, got: {}",
        balance_u128
    );
    tracing::info!("✓ Wallet has sufficient balance");

    // Step 4: Perform deposit
    tracing::info!("Step 4: Performing deposit of 100 tokens");
    let deposit_amount = 100u128;
    let deposit_result = deposit(&provider, &wallet, deposit_amount, &privacy_key).await?;

    tracing::info!("Deposit successful!");
    tracing::info!("  Transaction hash: {}", deposit_result.tx_hash);
    tracing::info!("  Rho: {}", hex::encode(&deposit_result.rho));
    tracing::info!("  Recipient: {}", hex::encode(&deposit_result.recipient));

    // Step 4a: Verify deposit result contains expected data
    assert!(
        !deposit_result.tx_hash.is_empty(),
        "Transaction hash should not be empty"
    );
    assert_ne!(deposit_result.rho, [0u8; 32], "Rho should not be all zeros");
    assert_ne!(
        deposit_result.recipient, [0u8; 32],
        "Recipient should not be all zeros"
    );
    tracing::info!("✓ Deposit completed successfully");

    // Step 5: Wait for deposit to be included in a block
    tracing::info!("Step 5: Waiting for deposit to be included (5 seconds)");
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    tracing::info!("✓ Wait completed");

    // Step 6: Initialize Ligero prover for transfer
    tracing::info!("Step 6: Initializing Ligero prover");
    let ligero = create_test_ligero();
    tracing::info!("✓ Ligero prover initialized");

    // Step 7: Perform transfer using deposit outputs
    tracing::info!("Step 7: Performing transfer using deposit outputs");
    let note_value = deposit_amount;
    let send_amount = deposit_amount; // Transfer the full amount (no change)
    let transfer_result = transfer(
        &ligero,
        &provider,
        &wallet,
        note_value,
        send_amount,
        deposit_result.rho,
        deposit_result.recipient,
        deposit_result.recipient, // Send to same recipient
        None, // No change since sending full amount
    )
    .await?;

    tracing::info!("Transfer successful!");
    tracing::info!("  Transaction hash: {}", transfer_result.tx_hash);
    tracing::info!("  Amount sent: {}", transfer_result.amount_sent);
    tracing::info!("  Output rho: {}", hex::encode(&transfer_result.output_rho));
    tracing::info!(
        "  Output recipient: {}",
        hex::encode(&transfer_result.output_recipient)
    );

    // Step 7a: Verify transfer result
    assert!(
        !transfer_result.tx_hash.is_empty(),
        "Transfer tx hash should not be empty"
    );
    assert_eq!(
        transfer_result.amount_sent, send_amount,
        "Amount sent should match send_amount"
    );
    assert_ne!(
        transfer_result.output_rho, [0u8; 32],
        "Output rho should not be all zeros"
    );
    assert_ne!(
        transfer_result.output_recipient, [0u8; 32],
        "Output recipient should not be all zeros"
    );
    assert_ne!(
        transfer_result.output_rho, deposit_result.rho,
        "Output rho should be different from input rho"
    );
    assert!(
        transfer_result.change_amount.is_none(),
        "Change amount should be None when sending full note"
    );
    tracing::info!("✓ Transfer completed successfully");

    // Final summary
    tracing::info!("==========================================");
    tracing::info!("✅ ALL TESTS PASSED!");
    tracing::info!("==========================================");
    tracing::info!("Summary:");
    tracing::info!("  - Wallet address: {}", wallet_address);
    tracing::info!("  - Initial balance: {}", balance_u128);
    tracing::info!("  - Deposit tx: {}", deposit_result.tx_hash);
    tracing::info!("  - Transfer tx: {}", transfer_result.tx_hash);
    tracing::info!("==========================================");

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
async fn test_wallet_address_format() -> Result<()> {
    let _ = dotenvy::dotenv();

    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let address = wallet.get_address();
    let address_str = address.to_string();

    tracing::info!("Wallet address: {}", address_str);

    // Check format
    assert!(
        address_str.starts_with("sov1"),
        "Address should start with 'sov1', got: {}",
        address_str
    );

    // Check length (typical Bech32 address length)
    assert!(
        address_str.len() > 10,
        "Address should be longer than 10 characters, got: {}",
        address_str.len()
    );

    tracing::info!("✅ Wallet address format is correct");

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services"]
async fn test_balance_check() -> Result<()> {
    let _ = dotenvy::dotenv();

    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");
    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running at ROLLUP_RPC_URL and VERIFIER_URL"
    );

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    let token_id: TokenId = config_gas_token_id();
    let address = wallet.get_address();
    let balance = provider.get_balance::<McpSpec>(&address, &token_id).await?;

    let balance_u128: u128 = balance.0;
    tracing::info!("Wallet balance: {}", balance_u128);

    assert!(
        balance_u128 > 2_000_000_000_000,
        "Balance should be > 2000000000000, got: {}",
        balance_u128
    );

    tracing::info!("✅ Balance check passed");

    Ok(())
}
