//! Integration tests for the MCP server
//!
//! These tests require a running rollup node, sequencer, and verifier service.
//! Make sure all services are running before executing these tests.
//! Environment variables (WALLET_PRIVATE_KEY, ROLLUP_RPC_URL, VERIFIER_URL, PRIVPOOL_SPEND_KEY)
//! should be set in .env (INDEXER_URL is optional, defaults to http://localhost:13100).

use anyhow::Result;
use demo_stf::runtime::Runtime;
use mcp_external::ligero::Ligero;
use mcp_external::operations::{deposit, transfer};
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::provider::Provider;
use mcp_external::wallet::WalletContext;
use sov_address::MultiAddressEvm;
use sov_bank::{config_gas_token_id, TokenId};
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;

type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;
use ligero_runner::LigeroRunner;

fn env_opt(var: &str) -> Option<std::path::PathBuf> {
    std::env::var(var).ok().map(std::path::PathBuf::from)
}

/// Helper to create test ligero prover (skips if assets are missing).
fn create_test_ligero() -> Option<Ligero> {
    let program = std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());

    let runner = LigeroRunner::new(&program);
    let prover = env_opt("LIGERO_PROVER_BIN")
        .or_else(|| env_opt("LIGERO_PROVER_BINARY_PATH"))
        .unwrap_or_else(|| runner.paths().prover_bin.clone());
    let shader =
        env_opt("LIGERO_SHADER_PATH").unwrap_or_else(|| std::path::PathBuf::from(runner.config().shader_path.clone()));

    if !prover.exists() || !shader.exists() {
        return None;
    }

    Some(Ligero::new(Some(prover), Some(shader), Some(program)))
}

/// Helper to check if services are available
async fn check_services_available(rpc_url: &str, verifier_url: &str, indexer_url: &str) -> bool {
    // Check rollup
    let provider_result = Provider::new(rpc_url, verifier_url, indexer_url).await;
    if provider_result.is_err() {
        eprintln!("⚠️  Rollup not available at {}", rpc_url);
        return false;
    }

    let provider = provider_result.unwrap();
    if !provider.is_healthy().await {
        eprintln!("⚠️  Rollup is not healthy");
        return false;
    }

    // Check verifier
    let verifier_check = reqwest::get(format!("{}/health", verifier_url)).await;
    if verifier_check.is_err() {
        eprintln!("⚠️  Verifier service not available at {}", verifier_url);
        return false;
    }

    true
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services and Ligero prover assets"]
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
    let Some(ligero) = create_test_ligero() else {
        eprintln!("⚠️  Skipping integration test: Ligero prover assets not found (set LIGERO_* env vars)");
        return Ok(());
    };
    tracing::info!("✓ Ligero prover initialized");

    // Step 7: Perform transfer using deposit outputs
    tracing::info!("Step 7: Performing transfer using deposit outputs");
    let note_value = deposit_amount; // Note value from deposit
    let send_amount = deposit_amount; // Transfer the full amount
    // Load authority VFK from environment for test
    let authority_vfk = std::env::var("AUTHORITY_VFK")
        .ok()
        .and_then(|s| {
            let trimmed = s.trim().strip_prefix("0x").unwrap_or(s.trim());
            hex::decode(trimmed).ok()
        })
        .and_then(|bytes| {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        });
    let transfer_result = transfer(
        &ligero,
        &provider,
        &wallet,
        *privacy_key
            .spend_sk()
            .expect("transfer requires spend_sk (privacy key must not be address-only)"),
        *privacy_key.pk(),
        note_value,
        send_amount,
        deposit_result.rho,
        deposit_result.recipient, // deposit convention: sender_id == recipient
        *privacy_key.pk(),
        *privacy_key.pk(),
        authority_vfk,
    )
    .await?;

    tracing::info!("Transfer successful!");
    tracing::info!("  Transaction hash: {}", transfer_result.tx_hash);
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

    let wallet_private_key = match std::env::var("WALLET_PRIVATE_KEY") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("⚠️  Skipping integration test: WALLET_PRIVATE_KEY not set");
            return Ok(());
        }
    };

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

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services and Ligero prover assets"]
async fn test_wallet_creation_deposit_and_send_flow() -> Result<()> {
    use midnight_privacy::FullViewingKey;
    use rand::RngCore;

    // Load .env file
    let _ = dotenvy::dotenv();

    tracing::info!("Starting wallet creation, deposit, and send integration test");

    // Get configuration from environment
    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");
    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());
    let startup_deposit_amount = 1000u128; // Test deposit amount

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running at ROLLUP_RPC_URL and VERIFIER_URL"
    );

    tracing::info!("Using ROLLUP_RPC_URL: {}", rpc_url);
    tracing::info!("Using VERIFIER_URL: {}", verifier_url);
    tracing::info!("Using INDEXER_URL: {}", indexer_url);
    tracing::info!("Test deposit amount: {}", startup_deposit_amount);

    // Step 1: Create funding wallet from private key
    tracing::info!("Step 1: Creating funding wallet");
    let funding_wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let funding_address = funding_wallet.get_address();
    tracing::info!("Funding wallet address: {}", funding_address);

    // Step 2: Connect to provider
    tracing::info!("Step 2: Connecting to rollup and verifier");
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;
    tracing::info!("✓ Connected to services");

    // Step 3: Generate new privacy key for test wallet
    tracing::info!("Step 3: Generating new privacy key");
    let mut rng = rand::thread_rng();
    let mut spend_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut spend_key_bytes);
    let new_privacy_key = PrivacyKey::from_hex(&hex::encode(spend_key_bytes))?;
    let new_privacy_address = new_privacy_key.privacy_address().to_string();
    tracing::info!("✓ New privacy address: {}", new_privacy_address);

    // Step 4: Generate new authority VFK
    tracing::info!("Step 4: Generating new authority VFK");
    let mut vfk_bytes = [0u8; 32];
    rng.fill_bytes(&mut vfk_bytes);
    let viewing_key = FullViewingKey(vfk_bytes);
    tracing::info!("✓ Authority VFK generated");

    // Step 5: Perform deposit to the new privacy address
    tracing::info!("Step 5: Depositing {} tokens to new privacy address", startup_deposit_amount);
    let deposit_result = deposit(&provider, &funding_wallet, startup_deposit_amount, &new_privacy_key).await?;
    tracing::info!("✓ Deposit successful");
    tracing::info!("  Transaction hash: {}", deposit_result.tx_hash);
    tracing::info!("  Rho: {}", hex::encode(&deposit_result.rho));

    // Step 6: Wait for deposit to be included
    tracing::info!("Step 6: Waiting for deposit to be included (10 seconds)");
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    tracing::info!("✓ Wait completed");

    // Step 7: Get initial wallet balance using get_unified_balance
    tracing::info!("Step 7: Getting initial wallet balance");
    let initial_balance_result = mcp_external::operations::get_unified_balance(
        &provider,
        &funding_wallet,
        mcp_external::server::DEFAULT_TOKEN_ID,
        &new_privacy_key,
        &viewing_key,
    )
    .await?;

    let initial_balance: u128 = initial_balance_result.privacy_balance.parse()?;
    tracing::info!("✓ Initial privacy balance: {}", initial_balance);

    // Validate initial balance matches deposit
    assert_eq!(
        initial_balance, startup_deposit_amount,
        "Initial balance {} should equal deposit amount {}",
        initial_balance, startup_deposit_amount
    );

    // Step 8: Send 50 tokens using transfer
    tracing::info!("Step 8: Sending 50 tokens");
    let send_amount = 50u128;

    // Initialize Ligero prover
    let Some(ligero) = create_test_ligero() else {
        eprintln!("⚠️  Skipping integration test: Ligero prover assets not found (set LIGERO_* env vars)");
        return Ok(());
    };

    // Get the deposited note details for transfer
    let note = initial_balance_result.unspent_notes.first()
        .expect("Should have at least one unspent note from deposit");

    // Parse rho from the note
    let input_rho_bytes = hex::decode(note.rho.trim_start_matches("0x"))?;
    let mut input_rho = [0u8; 32];
    input_rho.copy_from_slice(&input_rho_bytes);

    const DOMAIN: [u8; 32] = [1u8; 32];
    let input_recipient = new_privacy_key.recipient(&DOMAIN);

    // Use the viewing key bytes as authority VFK for the transfer
    let authority_vfk_for_transfer = Some(viewing_key.0);

    let transfer_result = transfer(
        &ligero,
        &provider,
        &funding_wallet,
        *new_privacy_key
            .spend_sk()
            .expect("transfer requires spend_sk (privacy key must not be address-only)"),
        *new_privacy_key.pk(),
        note.value,
        send_amount,
        input_rho,
        input_recipient, // deposit convention: sender_id == recipient
        *new_privacy_key.pk(),
        *new_privacy_key.pk(),
        authority_vfk_for_transfer,
    )
    .await?;

    tracing::info!("✓ Transfer successful");
    tracing::info!("  Transaction hash: {}", transfer_result.tx_hash);
    tracing::info!("  Amount sent: {}", transfer_result.amount_sent);

    // Step 9: Wait for transfer to complete
    tracing::info!("Step 9: Waiting for transfer to be included (10 seconds)");
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    tracing::info!("✓ Wait completed");

    // Step 10: Get final wallet balance
    tracing::info!("Step 10: Getting final wallet balance");
    let final_balance_result = mcp_external::operations::get_unified_balance(
        &provider,
        &funding_wallet,
        mcp_external::server::DEFAULT_TOKEN_ID,
        &new_privacy_key,
        &viewing_key,
    )
    .await?;

    let final_balance: u128 = final_balance_result.privacy_balance.parse()?;
    tracing::info!("✓ Final privacy balance: {}", final_balance);

    // Step 11: Validate final balance
    // Since we sent to ourselves, balance should still be the same
    assert_eq!(
        final_balance, initial_balance,
        "Final balance {} should equal initial balance {} (sent to ourselves)",
        final_balance, initial_balance
    );

    tracing::info!("✓ Balance validation passed");

    // Final summary
    tracing::info!("==========================================");
    tracing::info!("✅ ALL TESTS PASSED!");
    tracing::info!("==========================================");
    tracing::info!("Summary:");
    tracing::info!("  - New privacy address: {}", new_privacy_address);
    tracing::info!("  - Deposit amount: {}", startup_deposit_amount);
    tracing::info!("  - Initial balance: {}", initial_balance);
    tracing::info!("  - Amount sent: {}", send_amount);
    tracing::info!("  - Final balance: {}", final_balance);
    tracing::info!("  - Unspent notes: {}", final_balance_result.unspent_notes.len());
    tracing::info!("==========================================");

    Ok(())
}
