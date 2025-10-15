use anyhow::{Context, Result};
use sov_bank::TokenId;
use sov_cli::wallet_state::{AddressEntry, WalletState};
use sov_cli::workflows::keys::load_key;
use sov_cli::NodeClient;
use sov_modules_api::{Amount, CryptoSpec, DispatchCall, Spec};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Wallet context that holds the loaded wallet state and provides
/// access to wallet operations similar to sov-cli
#[allow(dead_code)]
pub struct WalletContext<Tx, S>
where
    Tx: DispatchCall,
    S: Spec,
{
    wallet_state: WalletState<Tx, S>,
    wallet_path: PathBuf,
    node_client: Option<Arc<NodeClient>>,
}

#[allow(dead_code)]
impl<Tx, S> WalletContext<Tx, S>
where
    Tx: DispatchCall,
    Tx::Decodable: serde::Serialize + serde::de::DeserializeOwned,
    S: Spec,
{
    /// Load wallet from a path and optionally connect to a node RPC
    pub async fn load(wallet_path: impl AsRef<Path>, rpc_url: Option<&str>) -> Result<Self> {
        let wallet_path = wallet_path.as_ref().to_path_buf();
        let wallet_state = WalletState::<Tx, S>::load(&wallet_path)
            .with_context(|| format!("Failed to load wallet from {}", wallet_path.display()))?;

        let node_client = if let Some(url) = rpc_url {
            Some(Arc::new(NodeClient::new(url).await.with_context(|| {
                format!("Failed to connect to node at {}", url)
            })?))
        } else {
            None
        };

        Ok(Self {
            wallet_state,
            wallet_path,
            node_client,
        })
    }

    /// Get the default (active) address entry
    pub fn default_address(&self) -> Option<&AddressEntry<S>> {
        self.wallet_state.addresses.default_address()
    }

    /// Get balance for the default address
    pub async fn get_default_balance(&self, token_id: &TokenId) -> Result<Amount> {
        let address_entry = self
            .default_address()
            .ok_or_else(|| anyhow::anyhow!("No default address in wallet"))?;

        self.get_balance(&address_entry.address, token_id).await
    }

    /// Get balance for a specific address
    pub async fn get_balance(&self, address: &S::Address, token_id: &TokenId) -> Result<Amount> {
        let client = self
            .node_client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No node client configured"))?;

        client
            .get_balance::<S>(address, token_id, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to get balance for token {} at address {:?}",
                    token_id, address
                )
            })
    }

    /// Get the public key of the default address
    pub fn default_public_key(&self) -> Result<<S::CryptoSpec as CryptoSpec>::PublicKey> {
        let address_entry = self
            .default_address()
            .ok_or_else(|| anyhow::anyhow!("No default address in wallet"))?;
        Ok(address_entry.pub_key.clone())
    }

    /// Load the private key for the default address
    pub fn load_default_private_key(&self) -> Result<<S::CryptoSpec as CryptoSpec>::PrivateKey> {
        let address_entry = self
            .default_address()
            .ok_or_else(|| anyhow::anyhow!("No default address in wallet"))?;

        load_key::<S>(&address_entry.location).with_context(|| {
            format!(
                "Failed to load key from {}",
                address_entry.location.display()
            )
        })
    }

    /// Get nonce for the default address
    pub async fn get_default_nonce(&self) -> Result<u64> {
        let address_entry = self
            .default_address()
            .ok_or_else(|| anyhow::anyhow!("No default address in wallet"))?;

        let client = self
            .node_client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No node client configured"))?;

        client
            .get_nonce_for_public_key::<S>(&address_entry.pub_key)
            .await
            .context("Failed to get nonce")
    }

    /// Get the node client
    pub fn node_client(&self) -> Option<&NodeClient> {
        self.node_client.as_deref()
    }
}

/// Thread-safe wrapper around WalletContext
#[allow(dead_code)]
pub type SharedWalletContext<Tx, S> = Arc<RwLock<WalletContext<Tx, S>>>;
