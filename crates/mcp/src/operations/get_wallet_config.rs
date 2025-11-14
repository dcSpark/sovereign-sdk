//! Get wallet configuration
//!
//! This module provides functionality for retrieving the wallet's configuration,
//! including RPC URL, wallet address, chain ID, and chain name.

use anyhow::Result;
use crate::provider::Provider;
use crate::wallet::WalletContext;

/// Wallet configuration information
#[derive(Debug, Clone, serde::Serialize)]
pub struct WalletConfig {
    /// RPC URL the wallet is connected to
    pub rpc_url: String,
    /// Wallet's default address
    pub address: String,
    /// Chain ID
    pub chain_id: u64,
    /// Chain name
    pub chain_name: String,
}

/// Get the wallet's configuration
///
/// Retrieves the configuration of the wallet, including the RPC URL,
/// wallet address, chain ID, and chain name.
///
/// # Parameters
/// * `provider` - The RPC provider for chain queries
/// * `wallet` - The wallet context containing the address
///
/// # Returns
/// Wallet configuration containing RPC URL, address, chain ID, and chain name
///
/// # Example
/// ```rust,no_run
/// # async fn example<Tx, S>(
/// #     provider: &mcp::provider::Provider,
/// #     wallet: &mcp::wallet::WalletContext<Tx, S>
/// # ) -> anyhow::Result<()>
/// # where
/// #     Tx: sov_modules_api::DispatchCall,
/// #     Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
/// #     S: sov_modules_api::Spec,
/// # {
/// use mcp::operations::get_wallet_config;
///
/// let config = get_wallet_config(provider, wallet).await?;
/// println!("Wallet address: {}", config.address);
/// println!("Connected to: {} (chain ID: {})", config.chain_name, config.chain_id);
/// # Ok(())
/// # }
/// ```
pub async fn get_wallet_config<Tx, S>(
    provider: &Provider,
    wallet: &WalletContext<Tx, S>,
) -> Result<WalletConfig>
where
    Tx: sov_modules_api::DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: sov_modules_api::Spec,
{
    tracing::info!("Getting wallet configuration");

    // Get the wallet's default address
    let address = crate::operations::get_default_address(wallet)?;

    // Get chain data from the provider
    let chain_data = provider.get_chain_data().await?;

    // Get RPC URL from provider
    let rpc_url = provider.rpc_url().to_string();

    let config = WalletConfig {
        rpc_url,
        address,
        chain_id: chain_data.chain_id,
        chain_name: chain_data.chain_name,
    };

    tracing::info!(
        "Wallet config retrieved - Address: {}, Chain: {} (ID: {}), RPC: {}",
        config.address,
        config.chain_name,
        config.chain_id,
        config.rpc_url
    );

    Ok(config)
}
