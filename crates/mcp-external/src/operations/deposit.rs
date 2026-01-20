//! Deposit operation for Midnight Privacy module

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use midnight_privacy::CallMessage as MidnightCallMessage;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;

use crate::operations::DEFAULT_MAX_FEE;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];

#[derive(Debug)]
pub struct DepositResult {
    pub tx_hash: String,
    #[allow(dead_code)]
    pub rho: [u8; 32],
    #[allow(dead_code)]
    pub recipient: [u8; 32],
}

pub async fn create_deposit_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    amount: u128,
    rho: [u8; 32],
    recipient: [u8; 32],
) -> Result<UnsignedTransaction<McpRuntime, McpSpec>> {
    let chain_data = provider
        .get_chain_data()
        .await
        .context("Failed to fetch chain data from rollup")?;

    let chain_id = chain_data.chain_id;

    tracing::info!("Using chain_id: {} ({})", chain_id, chain_data.chain_name);

    let deposit_call = MidnightCallMessage::<McpSpec>::Deposit {
        amount,
        rho,
        recipient,
        gas: None,
        view_fvks: None,
    };

    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::MidnightPrivacy(deposit_call);

    tracing::info!("Created runtime call for midnight_privacy deposit module");

    let public_key = wallet
        .default_public_key()
        .context("Failed to get public key from wallet")?;

    let nonce = provider
        .get_nonce::<McpSpec>(&public_key)
        .await
        .context("Failed to get nonce from provider")?;

    tracing::info!("Got nonce from rollup: {}", nonce);

    // Use timestamp as generation if nonce is 0
    let generation = if nonce == 0 {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        tracing::info!("Nonce is 0, using timestamp as generation: {}", timestamp);
        timestamp
    } else {
        tracing::info!("Using nonce as generation: {}", nonce);
        nonce
    };

    let max_fee = Amount::from(DEFAULT_MAX_FEE);

    let unsigned_tx = UnsignedTransaction::<McpRuntime, McpSpec>::new(
        runtime_call,
        chain_id,
        PriorityFeeBips::ZERO,
        max_fee,
        UniquenessData::Generation(generation),
        None,
    );

    tracing::debug!("Created unsigned transaction");

    Ok(unsigned_tx)
}

/// Deposit funds into the Midnight Privacy shielded pool
pub async fn deposit(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    amount: u128,
    privacy_key: &PrivacyKey,
) -> Result<DepositResult> {
    tracing::info!("Creating deposit for amount: {}", amount);

    let rho: [u8; 32] = rand::random();
    let recipient = privacy_key.recipient(&DOMAIN);
    let privacy_address = privacy_key.privacy_address(&DOMAIN);

    tracing::info!(
        "Depositing to privacy address: {} (recipient: {})",
        privacy_address,
        hex::encode(&recipient)
    );

    tracing::debug!(
        "Note parameters - rho: {}, recipient: {}",
        hex::encode(&rho),
        hex::encode(&recipient)
    );

    let unsigned_tx = create_deposit_unsigned_tx(provider, wallet, amount, rho, recipient).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    tracing::info!("Transaction signed and serialized: {} bytes", raw_tx.len());

    let tx_hash = provider
        .submit_to_verifier(raw_tx)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to verifier service")?;

    tracing::info!(
        "Deposit transaction submitted successfully via verifier: {}",
        tx_hash
    );

    Ok(DepositResult {
        tx_hash,
        rho,
        recipient,
    })
}
