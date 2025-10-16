//! Get the balance for a token ID using the default wallet address

use anyhow::{Context, Result};
use sov_bank::TokenId;
use sov_modules_api::{Amount, DispatchCall, Spec};

use crate::wallet::WalletContext;

/// Get the balance for a token ID using the default wallet address
pub async fn get_default_token_balance<Tx, S>(
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

    // Get the default address
    let address_entry = wallet
        .default_address()
        .context("No default address found in wallet")?;

    // Query the balance
    let balance = wallet
        .get_default_balance(&token_id_parsed)
        .await
        .with_context(|| {
            format!(
                "Failed to get balance for token {} at address {}",
                token_id, address_entry.address
            )
        })?;

    Ok((address_entry.address.to_string(), balance))
}

#[cfg(test)]
mod tests {
    // Tests will go here - these can use mock WalletContext
    // For now, we'll add integration tests that use real wallet files
}
