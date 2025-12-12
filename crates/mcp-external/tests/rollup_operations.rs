//! Integration tests that require a running rollup, verifier, and indexer services.
//! These are ignored by default; run with `cargo test --tests -- --ignored`.

use anyhow::Result;
use demo_stf::runtime::Runtime;
use mcp_external::operations::{deposit, get_transaction_status};
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::provider::Provider;
use mcp_external::wallet::WalletContext;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;

type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;

const TEST_PRIVATE_KEY_HEX: &str =
    "75fbf8d98746c2692e502942b938c82379fd09ea9f5b60d4d39e87e1b42468fd";
const DOMAIN: [u8; 32] = [1u8; 32];

fn rpc_urls() -> (String, String, String) {
    let rpc_url =
        std::env::var("ROLLUP_RPC_URL").unwrap_or_else(|_| "http://localhost:12346".to_string());
    let verifier_url =
        std::env::var("VERIFIER_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let indexer_url =
        std::env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".to_string());
    (rpc_url, verifier_url, indexer_url)
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/verifier/indexer services"]
async fn deposit_flow_submits_and_uses_privacy_key() -> Result<()> {
    let _ = dotenvy::dotenv();

    let (rpc_url, verifier_url, indexer_url) = rpc_urls();
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    let wallet = WalletContext::<McpRuntime, McpSpec>::from_private_key_hex(TEST_PRIVATE_KEY_HEX)?;

    let test_spend_sk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let privacy_key = PrivacyKey::from_hex(test_spend_sk)?;

    let amount = 100u128;
    let result = deposit(&provider, &wallet, amount, &privacy_key).await?;

    assert!(!result.tx_hash.is_empty(), "tx_hash should not be empty");
    assert_ne!(result.rho, [0u8; 32], "rho should be random");
    assert_eq!(
        result.recipient,
        privacy_key.recipient(&DOMAIN),
        "recipient should match derived privacy key recipient"
    );

    Ok(())
}

#[tokio::test]
#[tracing_test::traced_test]
#[ignore = "requires running rollup/indexer services"]
async fn transaction_status_returns_not_found_for_unknown_hash() -> Result<()> {
    let _ = dotenvy::dotenv();

    let (rpc_url, verifier_url, indexer_url) = rpc_urls();
    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;

    let missing_tx = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    let status = get_transaction_status(&provider, missing_tx).await;
    assert!(
        status.is_err(),
        "expected an error for unknown transaction but got {:?}",
        status
    );

    Ok(())
}
