//! Pool-admin operations for the Midnight Privacy module (freeze/unfreeze + admin set management).

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use midnight_privacy::{
    CallMessage as MidnightCallMessage, FrozenAddressesResponse, PrivacyAddress,
};
use sov_address::MultiAddressEvm;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::{Amount, Spec};
use sov_nightstream_adapter::Nightstream;

use crate::operations::DEFAULT_MAX_FEE;
use crate::provider::Provider;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, Nightstream, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

#[derive(Debug)]
pub struct AdminTxResult {
    pub tx_hash: String,
}

async fn create_midnight_privacy_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    call: MidnightCallMessage<McpSpec>,
) -> Result<UnsignedTransaction<McpRuntime, McpSpec>> {
    let chain_data = provider
        .get_chain_data()
        .await
        .context("Failed to fetch chain data from rollup")?;

    let chain_id = chain_data.chain_id;

    tracing::info!("Using chain_id: {} ({})", chain_id, chain_data.chain_name);

    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::MidnightPrivacy(call);

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

    Ok(UnsignedTransaction::<McpRuntime, McpSpec>::new(
        runtime_call,
        chain_id,
        PriorityFeeBips::ZERO,
        max_fee,
        UniquenessData::Generation(generation),
        None,
    ))
}

pub async fn freeze_address(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    address: PrivacyAddress,
) -> Result<AdminTxResult> {
    let call = MidnightCallMessage::<McpSpec>::FreezeAddress { address };
    let unsigned_tx = create_midnight_privacy_unsigned_tx(provider, wallet, call).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    let submit_result = provider
        .submit_to_verifier(raw_tx, None)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to verifier service")?;
    let tx_hash = submit_result.tx_hash;

    Ok(AdminTxResult { tx_hash })
}

pub async fn unfreeze_address(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    address: PrivacyAddress,
) -> Result<AdminTxResult> {
    let call = MidnightCallMessage::<McpSpec>::UnfreezeAddress { address };
    let unsigned_tx = create_midnight_privacy_unsigned_tx(provider, wallet, call).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    let submit_result = provider
        .submit_to_verifier(raw_tx, None)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to verifier service")?;
    let tx_hash = submit_result.tx_hash;

    Ok(AdminTxResult { tx_hash })
}

pub async fn add_pool_admin(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    admin: <McpSpec as Spec>::Address,
) -> Result<AdminTxResult> {
    let call = MidnightCallMessage::<McpSpec>::AddPoolAdmin { admin };
    let unsigned_tx = create_midnight_privacy_unsigned_tx(provider, wallet, call).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    let submit_result = provider
        .submit_to_verifier(raw_tx, None)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to verifier service")?;
    let tx_hash = submit_result.tx_hash;

    Ok(AdminTxResult { tx_hash })
}

pub async fn remove_pool_admin(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    admin: <McpSpec as Spec>::Address,
) -> Result<AdminTxResult> {
    let call = MidnightCallMessage::<McpSpec>::RemovePoolAdmin { admin };
    let unsigned_tx = create_midnight_privacy_unsigned_tx(provider, wallet, call).await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .inspect_err(|e| tracing::error!("Failed to sign transaction: {:?}", e))
        .context("Failed to sign transaction")?;

    let submit_result = provider
        .submit_to_verifier(raw_tx, None)
        .await
        .inspect_err(|e| tracing::error!("Failed to submit transaction: {:?}", e))
        .context("Failed to submit transaction to verifier service")?;
    let tx_hash = submit_result.tx_hash;

    Ok(AdminTxResult { tx_hash })
}

pub async fn list_frozen_addresses(provider: &Provider) -> Result<FrozenAddressesResponse> {
    provider
        .query_rest_endpoint("/modules/midnight-privacy/blacklist/frozen")
        .await
        .context("Failed to fetch frozen addresses from rollup")
}
