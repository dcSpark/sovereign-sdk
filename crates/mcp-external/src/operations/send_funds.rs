#![allow(dead_code)]
//! Send funds operation for Bank module

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use sov_address::MultiAddressEvm;
use sov_bank::{Coins, TokenId};
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;

use crate::operations::DEFAULT_MAX_FEE;
use crate::provider::Provider;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

#[derive(Debug)]
pub struct SendFundsResult {
    pub tx_hash: String,
}

pub async fn send_funds(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    to_address: &str,
    token_id: &TokenId,
    amount: Amount,
) -> Result<SendFundsResult> {
    tracing::info!("Sending {} to address: {}", amount, to_address);

    let to_address_parsed: <McpSpec as sov_modules_api::Spec>::Address = to_address
        .parse()
        .context("Failed to parse destination address")?;

    let coins = Coins {
        amount,
        token_id: token_id.clone(),
    };

    let bank_call = sov_bank::CallMessage::<McpSpec>::Transfer {
        to: to_address_parsed,
        coins,
    };

    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::Bank(bank_call);

    tracing::info!("Created runtime call for bank transfer");

    let unsigned_tx = create_bank_transfer_unsigned_tx(provider, wallet, runtime_call).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .context("Failed to sign transaction")?;

    tracing::info!("Transaction signed: {} bytes", raw_tx.len());

    let tx_hash = provider
        .submit_transaction(raw_tx)
        .await
        .context("Failed to submit transaction to rollup")?;

    tracing::info!("Transaction submitted successfully: {}", tx_hash);

    Ok(SendFundsResult { tx_hash })
}

async fn create_bank_transfer_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    runtime_call: demo_stf::runtime::RuntimeCall<McpSpec>,
) -> Result<UnsignedTransaction<McpRuntime, McpSpec>> {
    let chain_data = provider
        .get_chain_data()
        .await
        .context("Failed to fetch chain data from rollup")?;

    let chain_id = chain_data.chain_id;

    tracing::info!("Using chain_id: {} ({})", chain_id, chain_data.chain_name);

    let public_key = wallet
        .default_public_key()
        .context("Failed to get public key from wallet")?;

    let nonce = provider
        .get_nonce::<McpSpec>(&public_key)
        .await
        .context("Failed to get nonce from provider")?;

    tracing::info!("Got nonce from rollup: {}", nonce);

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
