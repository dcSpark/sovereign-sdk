//! Get the default wallet address as a string

use anyhow::{Context, Result};
use sov_modules_api::{DispatchCall, Spec};

use crate::wallet::WalletContext;

/// Get the default wallet address as a string
pub fn get_default_address<Tx, S>(wallet: &WalletContext<Tx, S>) -> Result<String>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    let address_entry = wallet
        .default_address()
        .context("No default address found in wallet")?;

    Ok(address_entry.address.to_string())
}

#[cfg(test)]
mod tests {
    // Tests will go here - these can use mock WalletContext
    // For now, we'll add integration tests that use real wallet files
}
