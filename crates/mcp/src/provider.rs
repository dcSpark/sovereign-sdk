//! RPC Provider for interacting with the Sovereign rollup
//!
//! This module handles all RPC communication with the rollup node, including:
//! - Chain state queries (nonce, balance)
//! - Transaction submission
//! - Fee estimation
//! - Block queries

use anyhow::{Context, Result};
use sov_bank::TokenId;
use sov_modules_api::{Amount, CryptoSpec, Spec};
use sov_node_client::NodeClient;
use std::sync::Arc;

/// Provider for RPC communication with the Sovereign rollup
///
/// Responsible for all network communication and chain state queries.
/// Does NOT handle wallet state or key management - that's WalletContext's job.
#[derive(Clone)]
pub struct Provider {
    client: Arc<NodeClient>,
}

impl Provider {
    /// Create a new provider connected to the given RPC URL
    pub async fn new(rpc_url: &str) -> Result<Self> {
        let client = NodeClient::new(rpc_url)
            .await
            .with_context(|| format!("Failed to connect to rollup node at {}", rpc_url))?;

        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Get the balance for a specific address and token
    pub async fn get_balance<S: Spec>(
        &self,
        address: &S::Address,
        token_id: &TokenId,
    ) -> Result<Amount> {
        self.client
            .get_balance::<S>(address, token_id, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to get balance for token {} at address {:?}",
                    token_id, address
                )
            })
    }

    /// Get the nonce for a public key
    pub async fn get_nonce<S: Spec>(
        &self,
        public_key: &<S::CryptoSpec as CryptoSpec>::PublicKey,
    ) -> Result<u64> {
        self.client
            .get_nonce_for_public_key::<S>(public_key)
            .await
            .context("Failed to get nonce from rollup")
    }

    /// Submit a raw transaction to the rollup sequencer
    ///
    /// This method accepts a borsh-serialized `Transaction` and submits it to the sequencer.
    /// Returns the transaction hash from the rollup.
    pub async fn submit_transaction(&self, raw_tx: Vec<u8>) -> Result<String> {
        let tx_hashes = self
            .client
            .send_transactions_to_sequencer(vec![raw_tx], false)
            .await
            .context("Failed to submit transaction to sequencer")?;

        let tx_hash = tx_hashes
            .first()
            .ok_or_else(|| anyhow::anyhow!("No transaction hash returned from sequencer"))?;

        // Convert TxHash to String (hex format)
        let bytes: &[u8] = tx_hash.as_ref();
        Ok(format!("0x{}", hex::encode(bytes)))
    }

    /// Get direct access to the underlying NodeClient
    ///
    /// Use this for advanced operations not covered by Provider methods
    #[allow(dead_code)]
    pub fn client(&self) -> &NodeClient {
        &self.client
    }
}
