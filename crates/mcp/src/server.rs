use std::sync::Arc;

use demo_stf::runtime::Runtime;
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
use tokio::sync::RwLock;

use crate::authority_vfk::AuthorityVfk;
use crate::ligero::Ligero as LigeroProver;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;
pub type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

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
    /// Random nonce (rho) used for the note
    pub rho: String,
    /// Recipient binding used for the note
    pub recipient: String,
}

// Types for Transfer
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct TransferRequest {
    /// Value of the note to transfer
    pub value: String,
    /// Rho (nonce) of the input note to spend (hex string)
    pub input_rho: String,
    /// Recipient of the input note to spend (hex string)
    pub input_recipient: String,
    /// Recipient for the output note (hex string)
    pub output_recipient: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct TransferResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
    /// New random nonce (rho) for the output note
    pub new_rho: String,
    /// New recipient binding for the output note
    pub new_recipient: String,
}

// Types for DecryptTransaction
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DecryptTransactionRequest {
    /// Transaction hash to decrypt (with or without 0x prefix)
    pub tx_hash: String,
    /// Optional VFK (32-byte hex string, with or without 0x prefix). Defaults to configured AUTHORITY_VFK.
    #[serde(default)]
    pub vfk: Option<String>,
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

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    provider: Option<Arc<Provider>>,
    wallet_context: Option<Arc<RwLock<McpWalletContext>>>,
    ligero_prover: Option<Arc<LigeroProver>>,
    authority_vfk: Option<Arc<AuthorityVfk>>,
    privacy_key: Arc<PrivacyKey>,
}

#[allow(rust_analyzer::macro_error)]
#[tool_router]
impl CryptoServer {
    pub fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
        authority_vfk: Option<Arc<AuthorityVfk>>,
        privacy_key: Arc<PrivacyKey>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context: Some(wallet_context),
            ligero_prover: Some(ligero_prover),
            authority_vfk,
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
            ErrorData::invalid_params(
                format!("Invalid token_id format: {}", e),
                None,
            )
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

        let viewing_key_bytes = if let Some(ref authority_vfk) = self.authority_vfk {
            *authority_vfk.as_bytes()
        } else {
            return Err(ErrorData::invalid_params(
                "Viewing key not configured. Set AUTHORITY_VFK to decrypt privacy pool notes.",
                None,
            ));
        };

        let viewing_key = midnight_privacy::FullViewingKey(viewing_key_bytes);

        let ctx = wallet_ctx.read().await;

        let unified_result = crate::operations::get_unified_balance(
            provider,
            &*ctx,
            DEFAULT_TOKEN_ID,
            &self.privacy_key,
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

        let privacy_address = self.privacy_key.privacy_address().to_string();

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
    #[tool(
        name = "getTransactions",
        description = "Get all transactions for the wallet. Retrieves a list of all transactions associated with the wallet."
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

        let transactions = crate::operations::get_transactions(provider, &*ctx)
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

        let config = crate::operations::get_wallet_config(provider, &*ctx, &self.privacy_key)
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

        let deposit_result = crate::operations::deposit(provider, &*ctx, amount, &self.privacy_key)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = DepositResult {
            tx_hash: deposit_result.tx_hash,
            rho: hex::encode(&deposit_result.rho),
            recipient: hex::encode(&deposit_result.recipient),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Transfer funds within the Midnight Privacy shielded pool.
    /// Creates a ZK proof to spend an existing note and creates a new output note.
    #[tool(
        name = "transfer",
        description = "Transfer funds within the Midnight Privacy shielded pool. Uses ZK proofs to spend a note and create a new output note. Requires `output_recipient` (32-byte hex) to direct the output to the desired recipient."
    )]
    async fn transfer(
        &self,
        Parameters(params): Parameters<TransferRequest>,
    ) -> Result<CallToolResult, ErrorData> {
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

        let ctx = wallet_ctx.read().await;

        // Parse value from string
        let value: u128 = params.value.parse().map_err(|_| {
            ErrorData::invalid_params("Invalid value format. Must be a valid u128 number.", None)
        })?;

        // Parse input_rho from hex string
        let input_rho_hex = params.input_rho.trim_start_matches("0x");
        let input_rho_bytes = hex::decode(input_rho_hex).map_err(|_| {
            ErrorData::invalid_params(
                "Invalid input_rho format. Must be a valid hex string.",
                None,
            )
        })?;

        if input_rho_bytes.len() != 32 {
            return Err(ErrorData::invalid_params(
                "input_rho must be exactly 32 bytes (64 hex characters).",
                None,
            ));
        }

        let mut input_rho = [0u8; 32];
        input_rho.copy_from_slice(&input_rho_bytes);

        // Parse input_recipient from hex string
        let input_recipient_hex = params.input_recipient.trim_start_matches("0x");
        let input_recipient_bytes = hex::decode(input_recipient_hex).map_err(|_| {
            ErrorData::invalid_params(
                "Invalid input_recipient format. Must be a valid hex string.",
                None,
            )
        })?;

        if input_recipient_bytes.len() != 32 {
            return Err(ErrorData::invalid_params(
                "input_recipient must be exactly 32 bytes (64 hex characters).",
                None,
            ));
        }

        let mut input_recipient = [0u8; 32];
        input_recipient.copy_from_slice(&input_recipient_bytes);

        let out_recipient_hex = params.output_recipient.trim_start_matches("0x");
        let out_recipient_bytes = hex::decode(out_recipient_hex).map_err(|_| {
            ErrorData::invalid_params(
                "Invalid output_recipient format. Must be a valid hex string.",
                None,
            )
        })?;

        if out_recipient_bytes.len() != 32 {
            return Err(ErrorData::invalid_params(
                "output_recipient must be exactly 32 bytes (64 hex characters).",
                None,
            ));
        }

        let mut output_recipient = [0u8; 32];
        output_recipient.copy_from_slice(&out_recipient_bytes);

        let transfer_result = crate::operations::transfer(
            ligero,
            provider,
            &*ctx,
            value,
            input_rho,
            input_recipient,
            output_recipient,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = TransferResult {
            tx_hash: transfer_result.tx_hash,
            new_rho: hex::encode(&transfer_result.new_rho),
            new_recipient: hex::encode(&transfer_result.new_recipient),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get transaction with selective privacy (authority view).
    /// Fetches the transaction from the indexer and decrypts encrypted notes using an authority VFK,
    /// enabling selective disclosure of transaction details while preserving privacy for others.
    #[tool(
        name = "getTransactionWithSelectivePrivacy",
        description = "Get transaction with selective privacy. Allows authorities to decrypt encrypted notes from a privacy transaction using their VFK (Viewing Full Key)."
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

        let vfk_hex = if let Some(ref provided) = params.vfk {
            provided.clone()
        } else if let Some(ref authority_vfk) = self.authority_vfk {
            hex::encode(authority_vfk.as_bytes())
        } else {
            return Err(ErrorData::invalid_params(
                "Viewing key not provided. Pass `vfk` or set AUTHORITY_VFK.",
                None,
            ));
        };

        let decrypt_result =
            crate::operations::decrypt_transaction(provider, &params.tx_hash, &vfk_hex)
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
