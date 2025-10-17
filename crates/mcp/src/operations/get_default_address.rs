//! Get the default wallet address as a string

use anyhow::Result;
use sov_modules_api::{DispatchCall, Spec};

use crate::wallet::WalletContext;

/// Get the default wallet address as a string
pub fn get_default_address<Tx, S>(wallet: &WalletContext<Tx, S>) -> Result<String>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    let address = wallet.get_address();
    Ok(address.to_string())
}

#[cfg(test)]
mod tests {
    // Tests will go here - these can use mock WalletContext
    // For now, we'll add integration tests that use real wallet files
}
