//! Get wallet configuration
//!
//! This module provides functionality for retrieving the wallet's configuration,
//! including RPC URL, wallet address, chain ID, and chain name.

use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::wallet::WalletContext;
use anyhow::Result;

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
    /// Privacy pool address for receiving shielded funds
    pub privacy_address: String,
}

/// Get the wallet's configuration
///
/// Retrieves the configuration of the wallet, including the RPC URL,
/// wallet address, chain ID, chain name, and privacy pool address.
///
/// # Parameters
/// * `provider` - The RPC provider for chain queries
/// * `wallet` - The wallet context containing the address
/// * `privacy_key` - The privacy key for deriving the privacy pool address
///
/// # Returns
/// Wallet configuration containing RPC URL, address, chain ID, chain name, and privacy address
///
/// # Example
/// ```rust,no_run
/// # async fn example<Tx, S>(
/// #     provider: &mcp::provider::Provider,
/// #     wallet: &mcp::wallet::WalletContext<Tx, S>,
/// #     privacy_key: &mcp::privacy_key::PrivacyKey
/// # ) -> anyhow::Result<()>
/// # where
/// #     Tx: sov_modules_api::DispatchCall,
/// #     Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
/// #     S: sov_modules_api::Spec,
/// # {
/// use mcp::operations::get_wallet_config;
///
/// let config = get_wallet_config(provider, wallet, privacy_key).await?;
/// println!("Wallet address: {}", config.address);
/// println!("Privacy address: {}", config.privacy_address);
/// println!("Connected to: {} (chain ID: {})", config.chain_name, config.chain_id);
/// # Ok(())
/// # }
/// ```
pub async fn get_wallet_config<Tx, S>(
    provider: &Provider,
    wallet: &WalletContext<Tx, S>,
    privacy_key: &PrivacyKey,
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

    // Get privacy address from privacy key
    let privacy_address = privacy_key.privacy_address().to_string();

    let config = WalletConfig {
        rpc_url,
        address,
        chain_id: chain_data.chain_id,
        chain_name: chain_data.chain_name,
        privacy_address: privacy_address.clone(),
    };

    tracing::info!(
        "Wallet config retrieved - Address: {}, Privacy Address: {}, Chain: {} (ID: {}), RPC: {}",
        config.address,
        config.privacy_address,
        config.chain_name,
        config.chain_id,
        config.rpc_url
    );

    Ok(config)
}
