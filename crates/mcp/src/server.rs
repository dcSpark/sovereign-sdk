use std::sync::Arc;

use demo_stf::runtime::Runtime;
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    // Re-exported derive crates (handy in derives below)
    schemars,
    serde,
    // Build-time macros & helpers
    tool,
    tool_handler,
    tool_router,
    // Types used by the server
    ErrorData,
    ServerHandler,
};
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::Spec;
use tokio::sync::RwLock;

use crate::fvk_service::{fetch_viewer_fvk_bundle, parse_hex_32, ViewerFvkBundle};
use crate::ligero::Ligero as LigeroProver;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;
pub type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SendFundsRequest {
    pub destination_address: String,
    pub amount: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SendFundsResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletAddressRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletAddressResult {
    /// Transparent wallet address
    pub address: String,
    /// Privacy pool address for receiving shielded funds
    pub privacy_address: String,
}

// Types for GetWalletBalance

/// Default gas token ID
pub const DEFAULT_TOKEN_ID: &str =
    "token_1nyl0e0yweragfsatygt24zmd8jrr2vqtvdfptzjhxkguz2xxx3vs0y07u7";

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletBalanceRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletBalanceResult {
    pub address: String,
    pub token_id: String,
    pub transparent_balance: String,
    pub privacy_balance: String,
    pub total_balance: String,
    pub unspent_notes: Vec<UnspentNoteInfo>,
    pub deposit_count: usize,
    pub transfer_count: usize,
    pub withdraw_count: usize,
    pub total_transactions_scanned: usize,
}

// Types for GetTransaction
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetTransactionRequest {
    /// Transaction hash ID (with or without 0x prefix)
    pub tx_hash: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionResult {
    /// Transaction hash
    pub tx_hash: String,
    /// Transaction status (e.g., "Success", "Failed", or "pending" if not yet indexed)
    pub status: String,
    /// Timestamp in milliseconds (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
    /// Transaction kind (e.g., "deposit", "withdraw", "transfer")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Sender address (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Recipient address (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// Transaction amount (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    /// Anchor root for privacy transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_root: Option<String>,
    /// Nullifier for privacy transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullifier: Option<String>,
    /// View Full Viewing Keys (FVKs) for note decryption
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_fvks: Option<serde_json::Value>,
    /// View attestations for privacy proofs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_attestations: Option<serde_json::Value>,
    /// Transaction events from the rollup
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<serde_json::Value>,
    /// Encrypted notes for privacy transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_notes: Option<serde_json::Value>,
    /// Full transaction payload
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

// Types for GetTransactions
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetTransactionsRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct TransactionInfo {
    /// Transaction hash
    pub tx_hash: String,
    /// Timestamp in milliseconds
    pub timestamp_ms: i64,
    /// Transaction kind (e.g., "deposit", "withdraw", "transfer")
    pub kind: String,
    /// Sender address (if available)
    pub sender: Option<String>,
    /// Recipient address (if available)
    pub recipient: Option<String>,
    /// Transaction amount (if available)
    pub amount: Option<String>,
    /// Anchor root for privacy transactions
    pub anchor_root: Option<String>,
    /// Nullifier for privacy transactions
    pub nullifier: Option<String>,
    /// View Full Viewing Keys (FVKs) for note decryption
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_fvks: Option<serde_json::Value>,
    /// View attestations for privacy proofs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_attestations: Option<serde_json::Value>,
    /// Transaction events from the rollup
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<serde_json::Value>,
    /// Transaction status (e.g., "Success", "Failed")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Encrypted notes for privacy transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_notes: Option<serde_json::Value>,
    /// Full transaction payload
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionsResult {
    /// List of transactions
    pub transactions: Vec<TransactionInfo>,
}

// Types for GetWalletConfig
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletConfigRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletConfigResult {
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
    /// Default token ID used for balance queries
    pub token_id: String,
}

// Types for Deposit
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DepositRequest {
    /// Amount to deposit into the shielded pool
    pub amount: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DepositResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
    /// FVK commitment (H("FVK_COMMIT_V1" || fvk)) - used to identify which viewing key can decrypt the note
    pub fvk_commitment: String,
    /// Recipient privacy address (bech32 format: privpool1...)
    pub recipient: String,
}

// Types for Transfer
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct TransferRequest {
    /// Transaction hash of the note to spend (from unspent_notes in walletBalance)
    pub note_tx_hash: String,
    /// Destination privacy address (bech32 format: privpool1...)
    pub destination_address: String,
    /// Amount to send. If less than note value, change is returned to your privacy address.
    /// If not provided, sends the full note value.
    #[serde(default)]
    pub amount: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct TransferResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
    /// Amount sent to destination
    pub amount_sent: String,
    /// Rho for the output note sent to destination
    pub output_rho: String,
    /// Recipient of the output note (bech32 privacy address: privpool1...)
    pub output_recipient: String,
    /// Change amount returned to sender (if partial transfer)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_amount: Option<String>,
    /// Rho for the change note (if partial transfer)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_rho: Option<String>,
    /// Recipient of change note - your privacy address (if partial transfer)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_recipient: Option<String>,
}

// Types for Pool Admin / Freeze (deny-map)
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FreezeAddressRequest {
    /// Privacy address to freeze (bech32m format: privpool1...)
    pub privacy_address: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct FreezeAddressResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UnfreezeAddressRequest {
    /// Privacy address to unfreeze (bech32m format: privpool1...)
    pub privacy_address: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct UnfreezeAddressResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddPoolAdminRequest {
    /// L2 address to grant pool-admin rights to
    pub admin_address: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct AddPoolAdminResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RemovePoolAdminRequest {
    /// L2 address to revoke pool-admin rights from
    pub admin_address: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RemovePoolAdminResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

// Types for DecryptTransaction
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DecryptTransactionRequest {
    /// Transaction hash to decrypt (with or without 0x prefix)
    pub tx_hash: String,
    /// Optional FVK (32-byte hex string, with or without 0x prefix). Defaults to the configured viewer FVK bundle.
    #[serde(default)]
    pub fvk: Option<String>,
}

/// Decrypted note information from a transaction
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DecryptedNoteInfo {
    /// Note domain
    pub domain: String,
    /// Token value/amount
    pub value: String,
    /// Note randomness (rho)
    pub rho: String,
    /// Recipient identifier
    pub recipient: String,
    /// Sender identifier (spender's address for transfers)
    /// - For deposit notes: None
    /// - For transfer notes: Some(sender_id)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DecryptTransactionResult {
    /// Transaction hash
    pub tx_hash: String,
    /// Transaction status
    pub status: String,
    /// Transaction kind
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Timestamp in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
    /// Decrypted notes from the transaction
    pub decrypted_notes: Vec<DecryptedNoteInfo>,
    /// Number of encrypted notes that were successfully decrypted
    pub decrypted_count: usize,
    /// Total number of encrypted notes in the transaction
    pub total_encrypted_notes: usize,
}

/// An unspent note in the privacy pool
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct UnspentNoteInfo {
    /// Note value
    pub value: String,
    /// Note rho (nonce) as hex string
    pub rho: String,
    /// Transaction hash where this note was created
    pub tx_hash: String,
    /// Timestamp when the note was created (milliseconds)
    pub timestamp_ms: i64,
    /// Transaction kind (deposit, transfer, withdraw)
    pub kind: String,
}

// Types for CreateWallet
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateWalletRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CreateWalletResult {
    /// New wallet private key (hex string)
    pub wallet_private_key: String,
    /// New wallet address
    pub wallet_address: String,
    /// New viewer FVK (hex string)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer_fvk: Option<String>,
    /// Viewer FVK commitment (hex string)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer_fvk_commitment: Option<String>,
    /// Pool signature (hex) over `viewer_fvk_commitment`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer_fvk_pool_sig_hex: Option<String>,
    /// Signer public key (hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer_fvk_signer_public_key: Option<String>,
    /// New privacy pool spending key (hex string)
    pub privacy_spend_key: String,
    /// New privacy pool address
    pub privacy_address: String,
}

// Types for RestoreWallet
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreWalletRequest {
    /// Wallet private key (hex string, with or without 0x prefix)
    pub wallet_private_key: String,
    /// Optional viewer FVK (hex string, with or without 0x prefix).
    ///
    /// If omitted and `POOL_FVK_PK` is set, a fresh viewer FVK will be requested from
    /// `midnight-fvk-service`.
    #[serde(default, alias = "authority_fvk", alias = "fvk")]
    pub viewer_fvk: Option<String>,
    /// Optional pool signature (hex) over the viewer FVK commitment.
    #[serde(
        default,
        alias = "pool_sig_hex",
        alias = "signature",
        alias = "pool_signature"
    )]
    pub viewer_fvk_pool_sig_hex: Option<String>,
    /// Privacy pool spending key (hex string, with or without 0x prefix)
    pub privacy_spend_key: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RestoreWalletResult {
    /// Restored wallet address
    pub wallet_address: String,
    /// Restored privacy pool address
    pub privacy_address: String,
}

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    provider: Option<Arc<Provider>>,
    wallet_context: Option<Arc<RwLock<McpWalletContext>>>,
    ligero_prover: Option<Arc<LigeroProver>>,
    viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
    privacy_key: Arc<RwLock<PrivacyKey>>,
}

#[allow(rust_analyzer::macro_error)]
#[tool_router]
impl CryptoServer {
    pub fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
        viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
        privacy_key: Arc<RwLock<PrivacyKey>>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context: Some(wallet_context),
            ligero_prover: Some(ligero_prover),
            viewer_fvk_bundle,
            privacy_key,
        }
    }

    /// Send funds to another wallet address using the Bank module.
    #[tool(
        name = "sendFunds",
        description = "Send funds to another wallet address on the L2. Creates and broadcasts a Bank transfer transaction to send tokens to a destination address. This is a standard L2 transfer, not a privacy pool transaction."
    )]
    async fn send_funds(
        &self,
        Parameters(params): Parameters<SendFundsRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        let amount: u128 = params.amount.parse().map_err(|_| {
            ErrorData::invalid_params("Invalid amount format. Must be a valid u128 number.", None)
        })?;

        let token_id: sov_bank::TokenId = DEFAULT_TOKEN_ID.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid token_id format: {}", e), None)
        })?;

        let amount_obj = sov_modules_api::Amount::from(amount);

        let send_result = crate::operations::send_funds(
            provider,
            &*ctx,
            &params.destination_address,
            &token_id,
            amount_obj,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = SendFundsResult {
            tx_hash: send_result.tx_hash,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the complete balance state (transparent L2 + privacy pool).
    /// Uses the default gas token for balance queries.
    /// Requires wallet context and privacy key to be configured.
    #[tool(
        name = "walletBalance",
        description = "Get the complete balance state of the wallet including transparent L2 balance and privacy pool balance."
    )]
    async fn wallet_balance(
        &self,
        Parameters(_params): Parameters<GetWalletBalanceRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let viewer_fvk_guard = self.viewer_fvk_bundle.read().await;
        let viewing_key_bytes = if let Some(ref bundle) = *viewer_fvk_guard {
            bundle.fvk
        } else {
            return Err(ErrorData::invalid_params(
                "Viewer key not configured. Set POOL_FVK_PK and ensure midnight-fvk-service is running.",
                None,
            ));
        };

        let viewing_key = midnight_privacy::FullViewingKey(viewing_key_bytes);

        let ctx = wallet_ctx.read().await;
        let privacy_key_guard = self.privacy_key.read().await;

        let unified_result = crate::operations::get_unified_balance(
            provider,
            &*ctx,
            DEFAULT_TOKEN_ID,
            &*privacy_key_guard,
            &viewing_key,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetWalletBalanceResult {
            address: unified_result.address,
            token_id: unified_result.token_id,
            transparent_balance: unified_result.transparent_balance,
            privacy_balance: unified_result.privacy_balance,
            total_balance: unified_result.total_balance,
            unspent_notes: unified_result
                .unspent_notes
                .into_iter()
                .map(|note| UnspentNoteInfo {
                    value: note.value.to_string(),
                    rho: note.rho,
                    tx_hash: note.tx_hash,
                    timestamp_ms: note.timestamp_ms,
                    kind: note.kind,
                })
                .collect(),
            deposit_count: unified_result.deposit_count,
            transfer_count: unified_result.transfer_count,
            withdraw_count: unified_result.withdraw_count,
            total_transactions_scanned: unified_result.total_transactions_scanned,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return the wallet's default address and privacy pool address.
    #[tool(
        name = "walletAddress",
        description = "Return the wallet's default address and privacy pool address for receiving shielded funds."
    )]
    async fn wallet_address(
        &self,
        Parameters(_params): Parameters<GetWalletAddressRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        let address = crate::operations::get_default_address(&*ctx)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let privacy_key_guard = self.privacy_key.read().await;
        let privacy_address = privacy_key_guard.privacy_address(&DOMAIN).to_string();

        let result = GetWalletAddressResult {
            address,
            privacy_address,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get full details of a transaction by its ID.
    /// Retrieves complete transaction information from the indexer including status, kind, amounts, and privacy fields.
    #[tool(
        name = "getTransaction",
        description = "Get transaction details by its ID. Retrieves complete transaction information including status, kind, amounts, and privacy-related fields."
    )]
    async fn get_transaction(
        &self,
        Parameters(params): Parameters<GetTransactionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

        let tx_details = crate::operations::get_transaction_status(provider, &params.tx_hash)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetTransactionResult {
            tx_hash: tx_details.tx_hash,
            status: tx_details.status,
            timestamp_ms: tx_details.timestamp_ms,
            kind: tx_details.kind,
            sender: tx_details.sender,
            recipient: tx_details.recipient,
            amount: tx_details.amount,
            anchor_root: tx_details.anchor_root,
            nullifier: tx_details.nullifier,
            view_fvks: tx_details.view_fvks,
            view_attestations: tx_details.view_attestations,
            events: tx_details.events,
            encrypted_notes: tx_details.encrypted_notes,
            payload: tx_details.payload,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get all transactions for the wallet.
    /// Queries both the normal wallet address (for deposits) and privacy address (for transfers).
    #[tool(
        name = "getTransactions",
        description = "Get all transactions for the wallet. Retrieves deposits from the L2 wallet address and transfers from/to the privacy address."
    )]
    async fn get_transactions(
        &self,
        Parameters(_params): Parameters<GetTransactionsRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;
        let privacy_key_guard = self.privacy_key.read().await;

        let transactions =
            crate::operations::get_transactions(provider, &*ctx, &*privacy_key_guard)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let transaction_infos: Vec<TransactionInfo> = transactions
            .into_iter()
            .map(|tx| TransactionInfo {
                tx_hash: tx.tx_hash,
                timestamp_ms: tx.timestamp_ms,
                kind: tx.kind,
                sender: tx.sender,
                recipient: tx.recipient,
                amount: tx.amount,
                anchor_root: tx.anchor_root,
                nullifier: tx.nullifier,
                view_fvks: tx.view_fvks,
                view_attestations: tx.view_attestations,
                events: tx.events,
                status: tx.status,
                encrypted_notes: tx.encrypted_notes,
                payload: tx.payload,
            })
            .collect();

        let result = GetTransactionsResult {
            transactions: transaction_infos,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the wallet's configuration.
    #[tool(
        name = "getWalletConfig",
        description = "Get the wallet's configuration. Retrieves the configuration of the wallet, including the RPC URL, wallet address, chain ID, chain name, and privacy pool address."
    )]
    async fn get_wallet_config(
        &self,
        Parameters(_params): Parameters<GetWalletConfigRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;
        let privacy_key_guard = self.privacy_key.read().await;

        let config = crate::operations::get_wallet_config(provider, &*ctx, &*privacy_key_guard)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetWalletConfigResult {
            rpc_url: config.rpc_url,
            address: config.address,
            chain_id: config.chain_id,
            chain_name: config.chain_name,
            privacy_address: config.privacy_address,
            token_id: DEFAULT_TOKEN_ID.to_string(),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Deposit funds into the Midnight Privacy shielded pool.
    /// Moves funds from the transparent balance into the privacy pool, creating a shielded note.
    #[tool(
        name = "deposit",
        description = "Deposit funds into the Midnight Privacy shielded pool. Moves funds from transparent balance to shielded balance."
    )]
    async fn deposit(
        &self,
        Parameters(params): Parameters<DepositRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        // Parse amount from string
        let amount: u128 = params.amount.parse().map_err(|_| {
            ErrorData::invalid_params("Invalid amount format. Must be a valid u128 number.", None)
        })?;

        let privacy_key_guard = self.privacy_key.read().await;

        let deposit_result =
            crate::operations::deposit(provider, &*ctx, amount, &*privacy_key_guard)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let recipient = privacy_key_guard.privacy_address(&DOMAIN).to_string();

        let fvk_commitment_hex = self
            .viewer_fvk_bundle
            .read()
            .await
            .as_ref()
            .map(|bundle| hex::encode(bundle.fvk_commitment))
            .unwrap_or_default();

        let result = DepositResult {
            tx_hash: deposit_result.tx_hash,
            fvk_commitment: fvk_commitment_hex,
            recipient,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Transfer funds within the Midnight Privacy shielded pool.
    /// Creates a ZK proof to spend an existing note and creates a new output note.
    /// Simply provide the tx_hash of the note to spend (from walletBalance unspent_notes) and the destination address.
    #[tool(
        name = "transfer",
        description = "Transfer funds within the Midnight Privacy shielded pool. Provide the tx_hash of the note to spend (from walletBalance unspent_notes) and the destination privacy address (privpool1...)."
    )]
    async fn transfer(
        &self,
        Parameters(params): Parameters<TransferRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        use midnight_privacy::PrivacyAddress;
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let ligero = self.ligero_prover.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Ligero prover not configured. Please set LIGERO_PROVER_BINARY_PATH and LIGERO_SHADER_PATH environment variables.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let viewer_fvk_bundle_for_transfer = self.viewer_fvk_bundle.read().await.clone();
        let viewing_key_bytes = viewer_fvk_bundle_for_transfer
            .as_ref()
            .map(|bundle| bundle.fvk)
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    "Viewer key not configured. Set POOL_FVK_PK and ensure midnight-fvk-service is running.",
                    None,
                )
            })?;
        let viewing_key = midnight_privacy::FullViewingKey(viewing_key_bytes);

        let ctx = wallet_ctx.read().await;
        let privacy_key_guard = self.privacy_key.read().await;

        // Get the wallet's unspent notes to find the note by tx_hash
        let unified_result = crate::operations::get_unified_balance(
            provider,
            &*ctx,
            DEFAULT_TOKEN_ID,
            &*privacy_key_guard,
            &viewing_key,
        )
        .await
        .map_err(|e| {
            ErrorData::internal_error(format!("Failed to get unspent notes: {}", e), None)
        })?;

        // Normalize the input tx_hash (add 0x prefix if missing, lowercase)
        let note_tx_hash = params.note_tx_hash.trim();
        let note_tx_hash_normalized =
            if note_tx_hash.starts_with("0x") || note_tx_hash.starts_with("0X") {
                note_tx_hash.to_lowercase()
            } else {
                format!("0x{}", note_tx_hash.to_lowercase())
            };

        // Find the note with matching tx_hash
        let note = unified_result
            .unspent_notes
            .iter()
            .find(|n| n.tx_hash.to_lowercase() == note_tx_hash_normalized)
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "Note with tx_hash {} not found in unspent notes. Use walletBalance to see available notes.",
                        note_tx_hash_normalized
                    ),
                    None,
                )
            })?;

        let note_value = note.value;

        // Determine send amount: if user specified, use it; otherwise send full note
        let send_amount: u128 = if let Some(ref amount_str) = params.amount {
            amount_str.parse().map_err(|_| {
                ErrorData::invalid_params(
                    "Invalid amount format. Must be a valid u128 number.",
                    None,
                )
            })?
        } else {
            note_value
        };

        // Validate amount
        if send_amount == 0 {
            return Err(ErrorData::invalid_params(
                "Amount must be greater than 0.",
                None,
            ));
        }
        if send_amount > note_value {
            return Err(ErrorData::invalid_params(
                format!("Amount {} exceeds note value {}", send_amount, note_value),
                None,
            ));
        }

        let has_change = send_amount < note_value;

        // Parse input_rho from the note
        let input_rho_hex = note.rho.trim_start_matches("0x");
        let input_rho_bytes = hex::decode(input_rho_hex).map_err(|_| {
            ErrorData::internal_error("Invalid rho in note. This should not happen.", None)
        })?;

        let mut input_rho = [0u8; 32];
        input_rho.copy_from_slice(&input_rho_bytes);

        // Input recipient is the current wallet's privacy address (derived from spend_sk).
        let input_recipient = privacy_key_guard.recipient(&DOMAIN);

        // Input sender_id must match the NOTE_V2 commitment for this note.
        // - Deposits: by convention sender_id = recipient (no sender_id in plaintext)
        // - Transfers/withdraw outputs: sender_id is included in decrypted plaintext
        let input_sender_id: [u8; 32] = if let Some(ref sender_id_hex) = note.sender_id {
            let bytes = hex::decode(sender_id_hex.trim_start_matches("0x")).map_err(|_| {
                ErrorData::internal_error(
                    "Invalid sender_id in note (expected hex-encoded 32 bytes)".to_string(),
                    None,
                )
            })?;
            if bytes.len() != 32 {
                return Err(ErrorData::internal_error(
                    "Invalid sender_id length in note (expected 32 bytes)".to_string(),
                    None,
                ));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            out
        } else {
            // Deposit-style note: sender_id is derived deterministically as recipient.
            input_recipient
        };

        let spend_sk = privacy_key_guard.spend_sk().ok_or_else(|| {
            ErrorData::invalid_params("Privacy key must include spend_sk to transfer.", None)
        })?;
        let spend_sk = *spend_sk;
        // v2 requires pk_ivk_owner as a private witness (derived from spend_sk + domain).
        let pk_ivk_owner = privacy_key_guard.pk_ivk(&DOMAIN);

        // Parse output recipient (destination bech32 privacy address)
        let output_privacy_addr: PrivacyAddress = params.destination_address.parse()
            .map_err(|e| ErrorData::invalid_params(
                format!("Invalid destination_address format. Must be a valid bech32 privacy address (privpool1...): {}", e),
                None,
            ))?;

        let destination_pk_spend = output_privacy_addr.to_pk();
        let destination_pk_ivk = output_privacy_addr.pk_ivk();

        tracing::info!(
            "[transfer] Spending note: tx_hash={}, note_value={}, send_amount={}, rho={}",
            note_tx_hash_normalized,
            note_value,
            send_amount,
            note.rho
        );
        tracing::info!(
            "[transfer] Destination: {}, has_change: {}",
            params.destination_address,
            has_change
        );

        let transfer_result = crate::operations::transfer(
            ligero,
            provider,
            &*ctx,
            spend_sk,
            pk_ivk_owner,
            note_value,
            send_amount,
            input_rho,
            input_sender_id,
            destination_pk_spend,
            destination_pk_ivk,
            viewer_fvk_bundle_for_transfer,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Get our own privacy address for change recipient display (reuse the guard from earlier)
        let my_privacy_address = privacy_key_guard.privacy_address(&DOMAIN).to_string();

        // Return the result with output and change details
        let result = TransferResult {
            tx_hash: transfer_result.tx_hash,
            amount_sent: transfer_result.amount_sent.to_string(),
            output_rho: hex::encode(&transfer_result.output_rho),
            output_recipient: output_privacy_addr.to_string(),
            change_amount: transfer_result.change_amount.map(|a| a.to_string()),
            change_rho: transfer_result.change_rho.map(|r| hex::encode(r)),
            change_recipient: if transfer_result.change_recipient.is_some() {
                Some(my_privacy_address)
            } else {
                None
            },
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Freeze a privacy address (pool admin only).
    /// Updates the on-chain deny-map so proofs for that identity stop verifying.
    #[tool(
        name = "freezeAddress",
        description = "Freeze (blacklist) a privacy pool address (privpool1...). Requires the caller to be a pool admin for the midnight-privacy module."
    )]
    async fn freeze_address(
        &self,
        Parameters(params): Parameters<FreezeAddressRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        use midnight_privacy::PrivacyAddress;

        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        let addr: PrivacyAddress = params.privacy_address.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid privacy address: {e}"), None)
        })?;

        let res = crate::operations::freeze_address(provider, &*ctx, addr)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&FreezeAddressResult {
            tx_hash: res.tx_hash,
        })
        .unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Unfreeze a privacy address (pool admin only).
    #[tool(
        name = "unfreezeAddress",
        description = "Unfreeze (un-blacklist) a privacy pool address (privpool1...). Requires the caller to be a pool admin for the midnight-privacy module."
    )]
    async fn unfreeze_address(
        &self,
        Parameters(params): Parameters<UnfreezeAddressRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        use midnight_privacy::PrivacyAddress;

        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        let addr: PrivacyAddress = params.privacy_address.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid privacy address: {e}"), None)
        })?;

        let res = crate::operations::unfreeze_address(provider, &*ctx, addr)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&UnfreezeAddressResult {
            tx_hash: res.tx_hash,
        })
        .unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Add a pool admin (module admin only).
    #[tool(
        name = "addPoolAdmin",
        description = "Grant pool-admin rights to an L2 address (module admin only). Pool admins can freeze/unfreeze privacy addresses."
    )]
    async fn add_pool_admin(
        &self,
        Parameters(params): Parameters<AddPoolAdminRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        let admin: <McpSpec as Spec>::Address = params
            .admin_address
            .parse()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid admin address: {e}"), None))?;

        let res = crate::operations::add_pool_admin(provider, &*ctx, admin)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&AddPoolAdminResult {
            tx_hash: res.tx_hash,
        })
        .unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Remove a pool admin (module admin only).
    #[tool(
        name = "removePoolAdmin",
        description = "Revoke pool-admin rights from an L2 address (module admin only)."
    )]
    async fn remove_pool_admin(
        &self,
        Parameters(params): Parameters<RemovePoolAdminRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let ctx = wallet_ctx.read().await;

        let admin: <McpSpec as Spec>::Address = params
            .admin_address
            .parse()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid admin address: {e}"), None))?;

        let res = crate::operations::remove_pool_admin(provider, &*ctx, admin)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&RemovePoolAdminResult {
            tx_hash: res.tx_hash,
        })
        .unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get transaction with selective privacy (authority view).
    /// Fetches the transaction from the indexer and decrypts encrypted notes using an authority FVK,
    /// enabling selective disclosure of transaction details while preserving privacy for others.
    #[tool(
        name = "getTransactionWithSelectivePrivacy",
        description = "Get transaction with selective privacy. Allows authorities to decrypt encrypted notes from a privacy transaction using their FVK (Full Viewing Key)."
    )]
    async fn get_transaction_with_selective_privacy(
        &self,
        Parameters(params): Parameters<DecryptTransactionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

        let viewer_fvk_guard = self.viewer_fvk_bundle.read().await;
        let fvk_hex = if let Some(ref provided) = params.fvk {
            provided.clone()
        } else if let Some(ref bundle) = *viewer_fvk_guard {
            hex::encode(bundle.fvk)
        } else {
            return Err(ErrorData::invalid_params(
                "Viewer key not configured. Pass `fvk` or set POOL_FVK_PK and run midnight-fvk-service.",
                None,
            ));
        };

        let decrypt_result =
            crate::operations::decrypt_transaction(provider, &params.tx_hash, &fvk_hex)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = DecryptTransactionResult {
            tx_hash: decrypt_result.tx_hash,
            status: decrypt_result.status,
            kind: decrypt_result.kind,
            timestamp_ms: decrypt_result.timestamp_ms,
            decrypted_notes: decrypt_result
                .decrypted_notes
                .into_iter()
                .map(|note| DecryptedNoteInfo {
                    domain: note.domain,
                    value: note.value.to_string(),
                    rho: note.rho,
                    recipient: note.recipient,
                    sender_id: note.sender_id,
                })
                .collect(),
            decrypted_count: decrypt_result.decrypted_count,
            total_encrypted_notes: decrypt_result.total_encrypted_notes,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Create a new wallet with new keys.
    /// Generates new wallet private key, viewer FVK (via midnight-fvk-service when POOL_FVK_PK is set),
    /// and privacy pool spending key.
    /// All subsequent transactions will use the new keys.
    #[tool(
        name = "createWallet",
        description = "Create a new wallet with new keys. Generates new wallet private key, viewer FVK (via midnight-fvk-service when POOL_FVK_PK is set), and privacy pool spending key. All subsequent operations will use the new keys."
    )]
    async fn create_wallet(
        &self,
        Parameters(_params): Parameters<CreateWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        use rand::RngCore;

        // Generate all random bytes first (before any async operations)
        // This ensures the RNG is dropped before any await points
        let (wallet_private_key_hex, privacy_spend_key_hex) = {
            let mut rng = rand::thread_rng();

            // Generate new wallet private key (32 bytes)
            let mut wallet_private_key_bytes = [0u8; 32];
            rng.fill_bytes(&mut wallet_private_key_bytes);
            let wallet_private_key_hex = hex::encode(&wallet_private_key_bytes);

            // Generate new privacy spend key (32 bytes)
            let mut privacy_spend_key_bytes = [0u8; 32];
            rng.fill_bytes(&mut privacy_spend_key_bytes);
            let privacy_spend_key_hex = hex::encode(&privacy_spend_key_bytes);

            (wallet_private_key_hex, privacy_spend_key_hex)
        }; // RNG is dropped here

        // Create new wallet context from the private key
        let new_wallet_ctx = McpWalletContext::from_private_key_hex(&wallet_private_key_hex)
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to create wallet context: {}", e), None)
            })?;

        let wallet_address = new_wallet_ctx.get_address().to_string();

        let pool_fvk_pk = std::env::var("POOL_FVK_PK")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| parse_hex_32("POOL_FVK_PK", &s))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid POOL_FVK_PK: {e}"), None))?;

        let viewer_fvk_bundle = if let Some(pool_pk) = pool_fvk_pk {
            let http = reqwest::Client::new();
            Some(fetch_viewer_fvk_bundle(&http, Some(pool_pk)).await.map_err(|e| {
                ErrorData::internal_error(
                    format!("Failed to fetch viewer FVK bundle from midnight-fvk-service: {e}"),
                    None,
                )
            })?)
        } else {
            None
        };

        // Create new privacy key
        let new_privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex).map_err(|e| {
            ErrorData::internal_error(format!("Failed to create privacy key: {}", e), None)
        })?;

        let privacy_address = new_privacy_key.privacy_address(&DOMAIN).to_string();

        // Replace the existing keys with the new ones
        if let Some(ref wallet_ctx) = self.wallet_context {
            let mut ctx_guard = wallet_ctx.write().await;
            *ctx_guard = new_wallet_ctx;
        }

        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        *viewer_fvk_guard = viewer_fvk_bundle.clone();

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = new_privacy_key;

        tracing::info!("[createWallet] New wallet created successfully");
        tracing::info!("[createWallet] Wallet address: {}", wallet_address);
        tracing::info!("[createWallet] Privacy address: {}", privacy_address);

        let result = CreateWalletResult {
            wallet_private_key: wallet_private_key_hex,
            wallet_address,
            viewer_fvk: viewer_fvk_bundle.as_ref().map(|b| hex::encode(b.fvk)),
            viewer_fvk_commitment: viewer_fvk_bundle
                .as_ref()
                .map(|b| hex::encode(b.fvk_commitment)),
            viewer_fvk_pool_sig_hex: viewer_fvk_bundle.as_ref().map(|b| b.pool_sig_hex.clone()),
            viewer_fvk_signer_public_key: viewer_fvk_bundle
                .as_ref()
                .map(|b| hex::encode(b.signer_public_key)),
            privacy_spend_key: privacy_spend_key_hex,
            privacy_address,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Restore a wallet from existing keys.
    /// Loads existing wallet private key and privacy pool spending key.
    ///
    /// If `POOL_FVK_PK` is set, a fresh viewer FVK bundle is requested from `midnight-fvk-service`.
    /// All subsequent transactions will use the restored keys.
    #[tool(
        name = "restoreWallet",
        description = "Restore a wallet from existing keys. Loads wallet private key and privacy pool spending key from hex strings. If POOL_FVK_PK is set, fetches a fresh viewer FVK from midnight-fvk-service. All subsequent operations will use the restored keys."
    )]
    async fn restore_wallet(
        &self,
        Parameters(params): Parameters<RestoreWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Strip 0x prefix if present
        let wallet_private_key_hex = params.wallet_private_key.trim_start_matches("0x");
        let privacy_spend_key_hex = params.privacy_spend_key.trim_start_matches("0x");

        // Validate hex strings are correct length (32 bytes = 64 hex chars)
        if wallet_private_key_hex.len() != 64 {
            return Err(ErrorData::invalid_params(
                "wallet_private_key must be exactly 32 bytes (64 hex characters).",
                None,
            ));
        }
        if privacy_spend_key_hex.len() != 64 {
            return Err(ErrorData::invalid_params(
                "privacy_spend_key must be exactly 32 bytes (64 hex characters).",
                None,
            ));
        }

        // Create wallet context from the private key
        let new_wallet_ctx = McpWalletContext::from_private_key_hex(wallet_private_key_hex)
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to create wallet context: {}", e), None)
            })?;

        let wallet_address = new_wallet_ctx.get_address().to_string();

        let pool_fvk_pk = std::env::var("POOL_FVK_PK")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| parse_hex_32("POOL_FVK_PK", &s))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid POOL_FVK_PK: {e}"), None))?;

        let viewer_fvk_bundle = if let Some(pool_pk) = pool_fvk_pk {
            let provided_fvk_hex = params
                .viewer_fvk
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let provided_sig_hex = params
                .viewer_fvk_pool_sig_hex
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());

            match (provided_fvk_hex, provided_sig_hex) {
                (Some(fvk_hex), Some(sig_hex)) => {
                    let fvk = parse_hex_32("viewer_fvk", fvk_hex).map_err(|e| {
                        ErrorData::invalid_params(format!("Invalid viewer_fvk: {e}"), None)
                    })?;

                    let sig_hex_trimmed = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
                    let sig_bytes = hex::decode(sig_hex_trimmed).map_err(|e| {
                        ErrorData::invalid_params(
                            format!("Invalid hex for viewer_fvk_pool_sig_hex: {e}"),
                            None,
                        )
                    })?;
                    if sig_bytes.len() != 64 {
                        return Err(ErrorData::invalid_params(
                            "viewer_fvk_pool_sig_hex must be 64 bytes (128 hex characters).",
                            None,
                        ));
                    }
                    let mut sig_arr = [0u8; 64];
                    sig_arr.copy_from_slice(&sig_bytes);

                    let commitment = midnight_privacy::fvk_commitment(&midnight_privacy::FullViewingKey(fvk));
                    let pool_vk = VerifyingKey::from_bytes(&pool_pk).map_err(|e| {
                        ErrorData::invalid_params(
                            format!("Invalid POOL_FVK_PK verifying key: {e}"),
                            None,
                        )
                    })?;
                    pool_vk
                        .verify_strict(&commitment, &Ed25519Signature::from_bytes(&sig_arr))
                        .map_err(|e| {
                            ErrorData::invalid_params(
                                format!(
                                    "Invalid viewer_fvk_pool_sig_hex for viewer_fvk_commitment: {e}"
                                ),
                                None,
                            )
                        })?;

                    Some(ViewerFvkBundle {
                        fvk,
                        fvk_commitment: commitment,
                        pool_sig_hex: sig_hex_trimmed.to_string(),
                        signer_public_key: pool_pk,
                    })
                }
                (None, None) => {
                    let http = reqwest::Client::new();
                    Some(fetch_viewer_fvk_bundle(&http, Some(pool_pk)).await.map_err(|e| {
                        ErrorData::internal_error(
                            format!(
                                "Failed to fetch viewer FVK bundle from midnight-fvk-service: {e}"
                            ),
                            None,
                        )
                    })?)
                }
                _ => {
                    return Err(ErrorData::invalid_params(
                        "When POOL_FVK_PK is set, restoreWallet must provide both viewer_fvk and viewer_fvk_pool_sig_hex (or neither to fetch a fresh one).",
                        None,
                    ));
                }
            }
        } else {
            None
        };

        // Create privacy key
        let new_privacy_key = PrivacyKey::from_hex(privacy_spend_key_hex).map_err(|e| {
            ErrorData::internal_error(format!("Failed to create privacy key: {}", e), None)
        })?;

        let privacy_address = new_privacy_key.privacy_address(&DOMAIN).to_string();

        // Replace the existing keys with the restored ones
        if let Some(ref wallet_ctx) = self.wallet_context {
            let mut ctx_guard = wallet_ctx.write().await;
            *ctx_guard = new_wallet_ctx;
        }

        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        *viewer_fvk_guard = viewer_fvk_bundle;

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = new_privacy_key;

        tracing::info!("[restoreWallet] Wallet restored successfully");
        tracing::info!("[restoreWallet] Wallet address: {}", wallet_address);
        tracing::info!("[restoreWallet] Privacy address: {}", privacy_address);

        let result = RestoreWalletResult {
            wallet_address,
            privacy_address,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for CryptoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Sovereign SDK MCP Server: Tools for querying wallet balances and interacting with Sovereign rollups.".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
