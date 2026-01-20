//! Comprehensive integration tests for all MCP tools
//!
//! These tests require a running rollup node, sequencer, and verifier service.
//! Make sure all services are running before executing these tests.

use anyhow::Result;
use demo_stf::runtime::Runtime;
use mcp_external::operations::{deposit, get_privacy_balance, verify_transaction};
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::provider::Provider;
use mcp_external::wallet::WalletContext;
use midnight_privacy::FullViewingKey;
use rand::RngCore;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;

type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];

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
#[ignore = "requires running rollup/verifier/indexer services"]
async fn test_get_wallet_config() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing getWalletConfig");

    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running"
    );

    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    // Test provider URLs
    assert_eq!(provider.rpc_url(), &rpc_url);
    assert_eq!(provider.indexer_url(), &indexer_url);

    // Test chain data
    let chain_data = provider.get_chain_data().await?;
    assert!(
        !chain_data.chain_name.is_empty(),
        "Chain name should not be empty"
    );

    tracing::info!("✅ getWalletConfig test passed");
    tracing::info!("  RPC URL: {}", rpc_url);
    tracing::info!("  Indexer URL: {}", indexer_url);
    tracing::info!("  Chain: {}", chain_data.chain_name);

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services"]
async fn test_wallet_address() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing walletAddress");

    // Generate a new privacy key
    let mut rng = rand::thread_rng();
    let mut spend_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut spend_key_bytes);
    let privacy_key = PrivacyKey::from_hex(&hex::encode(spend_key_bytes))?;

    // Get privacy address
    let privacy_address = privacy_key.privacy_address(&DOMAIN);
    let address_str = privacy_address.to_string();

    // Validate address format
    assert!(
        address_str.starts_with("privpool1"),
        "Privacy address should start with 'privpool1', got: {}",
        address_str
    );
    assert!(
        address_str.len() > 15,
        "Privacy address should be longer than 15 characters, got: {}",
        address_str.len()
    );

    tracing::info!("✅ walletAddress test passed");
    tracing::info!("  Privacy address: {}", address_str);

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services and Ligero prover assets"]
async fn test_get_transaction() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing getTransaction");

    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");
    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running"
    );

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    // Generate privacy key for deposit
    let mut rng = rand::thread_rng();
    let mut spend_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut spend_key_bytes);
    let privacy_key = PrivacyKey::from_hex(&hex::encode(spend_key_bytes))?;

    // Perform a deposit to create a transaction
    let deposit_amount = 50u128;
    let deposit_result = deposit(&provider, &wallet, deposit_amount, &privacy_key).await?;
    let tx_hash = deposit_result.tx_hash.clone();

    tracing::info!("Created deposit transaction: {}", tx_hash);

    // Wait for transaction to be indexed
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Get the transaction
    let tx_option = provider.get_transaction(&tx_hash).await?;

    assert!(tx_option.is_some(), "Transaction should exist in indexer");

    let tx = tx_option.unwrap();
    assert_eq!(tx.tx_hash, tx_hash, "Transaction hash should match");
    assert_eq!(tx.kind, "deposit", "Transaction kind should be deposit");

    tracing::info!("✅ getTransaction test passed");
    tracing::info!("  Transaction hash: {}", tx.tx_hash);
    tracing::info!("  Kind: {}", tx.kind);
    tracing::info!("  Status: {:?}", tx.status);

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services and Ligero prover assets"]
async fn test_verify_transaction() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing verifyTransaction");

    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");
    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running"
    );

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    // Generate privacy key and FVK
    let mut rng = rand::thread_rng();
    let mut spend_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut spend_key_bytes);
    let privacy_key = PrivacyKey::from_hex(&hex::encode(spend_key_bytes))?;

    let mut fvk_bytes = [0u8; 32];
    rng.fill_bytes(&mut fvk_bytes);
    let fvk_hex = hex::encode(&fvk_bytes);

    // Perform a deposit
    let deposit_amount = 75u128;
    let deposit_result = deposit(&provider, &wallet, deposit_amount, &privacy_key).await?;
    let tx_hash = deposit_result.tx_hash.clone();

    tracing::info!("Created deposit transaction: {}", tx_hash);

    // Wait for transaction to be indexed
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Verify the transaction exists
    let verify_result = verify_transaction(&provider, &tx_hash, Some(&fvk_hex)).await?;

    assert!(verify_result.exists, "Transaction should exist");
    // Note: amount might be "encrypted" if FVK doesn't match, or the actual amount if decryption works
    assert!(
        !verify_result.transaction_amount.is_empty(),
        "Transaction amount should not be empty"
    );

    tracing::info!("✅ verifyTransaction test passed");
    tracing::info!("  Transaction exists: {}", verify_result.exists);
    tracing::info!("  Amount: {}", verify_result.transaction_amount);

    // Test non-existent transaction
    let fake_tx_hash = "0x0000000000000000000000000000000000000000000000000000000000000000";
    let verify_result_fake = verify_transaction(&provider, fake_tx_hash, Some(&fvk_hex)).await?;

    assert!(
        !verify_result_fake.exists,
        "Fake transaction should not exist"
    );
    assert_eq!(
        verify_result_fake.transaction_amount, "0",
        "Non-existent transaction should have 0 amount"
    );

    tracing::info!("✅ Non-existent transaction verification passed");

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services and Ligero prover assets"]
async fn test_get_privacy_balance() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing walletBalance (via get_privacy_balance)");

    let wallet_private_key =
        std::env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY must be set in .env");
    let rpc_url = std::env::var("ROLLUP_RPC_URL").expect("ROLLUP_RPC_URL must be set in .env");
    let verifier_url = std::env::var("VERIFIER_URL").expect("VERIFIER_URL must be set in .env");
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());

    assert!(
        check_services_available(&rpc_url, &verifier_url, &indexer_url).await,
        "Required services must be running"
    );

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    // Generate privacy key and FVK
    let mut rng = rand::thread_rng();
    let mut spend_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut spend_key_bytes);
    let privacy_key = PrivacyKey::from_hex(&hex::encode(spend_key_bytes))?;

    let mut fvk_bytes = [0u8; 32];
    rng.fill_bytes(&mut fvk_bytes);
    let viewing_key = FullViewingKey(fvk_bytes);

    // Get initial balance (should be 0)
    let initial_balance = get_privacy_balance(&provider, &privacy_key, Some(&viewing_key)).await?;

    let initial_privacy_balance = initial_balance.balance;
    tracing::info!("Initial privacy balance: {}", initial_privacy_balance);

    // Perform a deposit
    let deposit_amount = 100u128;
    let deposit_result = deposit(&provider, &wallet, deposit_amount, &privacy_key).await?;

    tracing::info!("Deposit successful: {}", deposit_result.tx_hash);

    // Wait for deposit to be indexed
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Get balance after deposit
    let final_balance = get_privacy_balance(&provider, &privacy_key, Some(&viewing_key)).await?;

    let final_privacy_balance = final_balance.balance;
    tracing::info!("Final privacy balance: {}", final_privacy_balance);

    // Balance should have increased
    assert_eq!(
        final_privacy_balance,
        initial_privacy_balance + deposit_amount,
        "Balance should increase by deposit amount"
    );

    // Check unspent notes
    assert!(
        !final_balance.unspent_notes.is_empty(),
        "Should have at least one unspent note"
    );

    tracing::info!("✅ walletBalance test passed");
    tracing::info!("  Initial balance: {}", initial_privacy_balance);
    tracing::info!("  Final balance: {}", final_privacy_balance);
    tracing::info!("  Unspent notes: {}", final_balance.unspent_notes.len());

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
async fn test_restore_wallet_keys() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing restoreWallet (key restoration)");

    // Generate test keys
    let mut rng = rand::thread_rng();

    // Generate wallet private key
    let mut wallet_private_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut wallet_private_key_bytes);
    let wallet_private_key_hex = hex::encode(&wallet_private_key_bytes);

    // Generate authority FVK
    let mut authority_fvk_bytes = [0u8; 32];
    rng.fill_bytes(&mut authority_fvk_bytes);
    let _authority_fvk_hex = hex::encode(&authority_fvk_bytes);

    // Generate privacy spend key
    let mut privacy_spend_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut privacy_spend_key_bytes);
    let privacy_spend_key_hex = hex::encode(&privacy_spend_key_bytes);

    // Test restoring wallet context
    let wallet =
        WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key_hex)?;
    let wallet_address = wallet.get_address().to_string();

    // Test restoring privacy key
    let privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex)?;
    let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

    // Validate
    assert!(
        wallet_address.starts_with("sov1"),
        "Wallet address should start with 'sov1'"
    );
    assert!(
        privacy_address.starts_with("privpool1"),
        "Privacy address should start with 'privpool1'"
    );

    tracing::info!("✅ restoreWallet test passed");
    tracing::info!("  Wallet address: {}", wallet_address);
    tracing::info!("  Privacy address: {}", privacy_address);

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
async fn test_privacy_key_formats() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Testing privacy key format support");

    // Test hex format
    let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let privacy_key_1 = PrivacyKey::from_hex(hex_key)?;

    // Test hex format with 0x prefix
    let hex_key_with_prefix = format!("0x{}", hex_key);
    let privacy_key_2 = PrivacyKey::from_hex(&hex_key_with_prefix)?;

    // Both should produce same address
    assert_eq!(
        privacy_key_1.privacy_address(&DOMAIN).to_string(),
        privacy_key_2.privacy_address(&DOMAIN).to_string(),
        "Both hex formats should produce the same address"
    );

    // Test bech32m address format
    let privacy_address = privacy_key_1.privacy_address(&DOMAIN).to_string();
    let privacy_key_3 = PrivacyKey::from_address(&privacy_address)?;

    assert_eq!(
        privacy_key_1.privacy_address(&DOMAIN).to_string(),
        privacy_key_3.privacy_address(&DOMAIN).to_string(),
        "Address format should roundtrip correctly"
    );

    tracing::info!("✅ Privacy key format test passed");
    tracing::info!("  Hex format: ✓");
    tracing::info!("  Hex with 0x prefix: ✓");
    tracing::info!("  Bech32m address format: ✓");

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services and Ligero prover assets"]
async fn test_verify_transaction_shows_sent_not_sum() -> Result<()> {
    // This test validates that verifyTransaction returns the sent amount,
    // not the sum of all decrypted notes (sent + change).
    //
    // Scenario:
    // 1. Deposit 500 to create a note
    // 2. Transfer 100 to receiver (creates 2 notes: 100 sent, 400 change)
    // 3. Verify the transfer transaction
    // 4. Expected: verifyTransaction should return 100 (sent amount)
    //    Bug: Currently returns 500 (100 + 400 = sum of all notes)

    let _ = dotenvy::dotenv();

    tracing::info!("Testing verifyTransaction - should return sent amount, not sum of all notes");
    tracing::info!("Issue: When verifying a transfer, the function sums all decrypted notes");
    tracing::info!("Expected: Return the first note's value (the sent amount)");

    // For now, this is a placeholder test that documents the expected behavior
    // The actual fix is in src/operations/verify_transaction.rs

    Ok(())
}
