//! Get the balance for a token ID using the default wallet address

use anyhow::{Context, Result};
use sov_bank::TokenId;
use sov_modules_api::{Amount, DispatchCall, Spec};

use crate::provider::Provider;
use crate::wallet::WalletContext;

/// Get the balance for a token ID using the default wallet address
///
/// # Parameters
/// * `provider` - The RPC provider for querying chain state
/// * `wallet` - The wallet context for address information
/// * `token_id` - The token ID as a bech32 string
pub async fn get_default_token_balance<Tx, S>(
    provider: &Provider,
    wallet: &WalletContext<Tx, S>,
    token_id: &str,
) -> Result<(String, Amount)>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    // Parse the token ID
    let token_id_parsed: TokenId = token_id
        .parse()
        .with_context(|| format!("Invalid token ID format: {}", token_id))?;

    // Get the default address from wallet
    let address = wallet.get_address();

    // Query the balance from provider
    let balance = provider
        .get_balance::<S>(&address, &token_id_parsed)
        .await
        .with_context(|e| {
            tracing::error!(
                "Failed to get balance for token {} at address {}: {}",
                token_id, address, e
            );
            e.to_string()
        })?;

    Ok((address.to_string(), balance))
}

#[cfg(test)]
mod tests {
    // Tests will go here - these can use mock WalletContext
    // For now, we'll add integration tests that use real wallet files
}
