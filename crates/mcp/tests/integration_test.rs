//! Integration tests for the MCP server
//!
//! These tests require a running rollup node, sequencer, and verifier service.
//! Make sure all services are running before executing these tests.
//! Environment variables (WALLET_PRIVATE_KEY, ROLLUP_RPC_URL, VERIFIER_URL, PRIVPOOL_SPEND_KEY)
//! should be set in .env (INDEXER_URL is optional, defaults to http://localhost:13100).

use anyhow::Result;
use demo_stf::runtime::Runtime;
use mcp::operations::{deposit, transfer};
use mcp::privacy_key::PrivacyKey;
use mcp::provider::Provider;
use mcp::test_utils::nightstream::create_test_nightstream;
use mcp::wallet::WalletContext;
use sov_address::MultiAddressEvm;
use sov_bank::{config_gas_token_id, TokenId};
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_nightstream_adapter::Nightstream as NightstreamAdapter;

type McpSpec = ConfigurableSpec<MockDaSpec, NightstreamAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];

/// Helper to check if services are available
async fn check_services_available(rpc_url: &str, verifier_url: &str, indexer_url: &str) -> bool {
    let provider_result = Provider::new(rpc_url, verifier_url, indexer_url).await;
    if provider_result.is_err() {
        eprintln!("⚠️  Rollup not available at {}", rpc_url);
        return false;
    }

    let _provider = provider_result.unwrap();

    let verifier_check = reqwest::get(format!("{}/health", verifier_url)).await;
    if verifier_check.is_err() {
        eprintln!("⚠️  Verifier service not available at {}", verifier_url);
        return false;
    }

    true
}

#[tokio::test]
#[tracing_test::traced_test]
async fn test_deposit_and_transfer_flow() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing::info!("Starting deposit and transfer integration test");

    let wallet_private_key = match std::env::var("WALLET_PRIVATE_KEY") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("⚠️  Skipping integration test: WALLET_PRIVATE_KEY not set");
            return Ok(());
        }
    };
    let rpc_url = match std::env::var("ROLLUP_RPC_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("⚠️  Skipping integration test: ROLLUP_RPC_URL not set");
            return Ok(());
        }
    };
    let verifier_url = match std::env::var("VERIFIER_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("⚠️  Skipping integration test: VERIFIER_URL not set");
            return Ok(());
        }
    };
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());
    let privpool_spend_key = match std::env::var("PRIVPOOL_SPEND_KEY") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("⚠️  Skipping integration test: PRIVPOOL_SPEND_KEY not set");
            return Ok(());
        }
    };

    if !check_services_available(&rpc_url, &verifier_url, &indexer_url).await {
        eprintln!("⚠️  Skipping integration test: required services not available");
        return Ok(());
    }

    let privacy_key = if privpool_spend_key.starts_with("privpool1") {
        PrivacyKey::from_address(&privpool_spend_key)
    } else {
        PrivacyKey::from_hex(&privpool_spend_key)
    }
    .expect("Failed to parse PRIVPOOL_SPEND_KEY");

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(&wallet_private_key)?;
    let wallet_address = wallet.get_address();
    let wallet_address_str = wallet_address.to_string();

    assert!(
        wallet_address_str.starts_with("sov1"),
        "Wallet address should start with 'sov1', got: {}",
        wallet_address_str
    );

    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    let token_id: TokenId = config_gas_token_id();
    let balance = provider
        .get_balance::<McpSpec>(&wallet_address, &token_id)
        .await?;

    let balance_u128: u128 = balance.0;
    assert!(
        balance_u128 > 2_000_000_000_000,
        "Wallet balance should be greater than 2000000000000, got: {}",
        balance_u128
    );

    let deposit_amount = 100u128;
    let deposit_result = deposit(&provider, &wallet, deposit_amount, &privacy_key).await?;

    assert!(!deposit_result.tx_hash.is_empty());
    assert_ne!(deposit_result.rho, [0u8; 32]);
    assert_ne!(deposit_result.recipient, [0u8; 32]);

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let Some(nightstream) = create_test_nightstream() else {
        eprintln!(
            "⚠️  Skipping integration test: Nightstream proof service not configured (set NIGHTSTREAM_PROOF_SERVICE_URL)"
        );
        return Ok(());
    };

    let note_value = deposit_amount;
    let send_amount = deposit_amount;
    let spend_sk = match privacy_key.spend_sk() {
        Some(sk) => *sk,
        None => {
            eprintln!("⚠️  Skipping integration test: PRIVPOOL_SPEND_KEY must be a spend_sk to run transfers");
            return Ok(());
        }
    };
    let pk_ivk_owner = privacy_key.pk_ivk(&DOMAIN);
    let destination_pk_spend = *privacy_key.pk();
    let destination_pk_ivk = privacy_key.pk_ivk(&DOMAIN);
    let input_sender_id = deposit_result.recipient;

    let transfer_result = transfer(
        &nightstream,
        &provider,
        &wallet,
        spend_sk,
        pk_ivk_owner,
        note_value,
        send_amount,
        deposit_result.rho,
        input_sender_id,
        destination_pk_spend,
        destination_pk_ivk,
        None,
    )
    .await?;

    assert!(!transfer_result.tx_hash.is_empty());
    assert_eq!(transfer_result.amount_sent, send_amount);
    assert_ne!(transfer_result.output_rho, [0u8; 32]);
    assert_ne!(transfer_result.output_recipient, [0u8; 32]);
    assert_ne!(transfer_result.output_rho, deposit_result.rho);
    assert!(transfer_result.change_amount.is_none());

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

    assert!(
        address_str.starts_with("sov1"),
        "Address should start with 'sov1', got: {}",
        address_str
    );
    assert!(
        address_str.len() > 10,
        "Address should be longer than 10 characters, got: {}",
        address_str.len()
    );

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
    assert!(
        balance_u128 > 2_000_000_000_000,
        "Balance should be > 2000000000000, got: {}",
        balance_u128
    );

    Ok(())
}
