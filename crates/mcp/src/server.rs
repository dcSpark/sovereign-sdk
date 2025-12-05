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

use crate::authority_fvk::AuthorityFvk;
use crate::ligero::Ligero as LigeroProver;
use crate::provider::Provider;
use crate::wallet::WalletContext;

// Define the concrete spec type used by the MCP server
// This matches the MockRollupSpec from rollup-ligero
pub type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;
// WalletContext takes the DispatchCall type itself, not its Decodable
pub type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

// -----------------------------
// Types for SendFunds
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SendFundsRequest {
    /// The recipient's wallet address (currently ignored, will be used in future implementation)
    pub destination_address: String,
    /// The amount to send
    pub amount: i64,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SendFundsResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

// -----------------------------
// Types for GetWalletAddress
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletAddressRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletAddressResult {
    pub address: String,
}

// -----------------------------
// Types for GetWalletBalance
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletBalanceRequest {
    /// Token ID to query (as bech32 string, e.g., "token_1...")
    pub token_id: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletBalanceResult {
    pub address: String,
    pub token_id: String,
    pub balance: String,
}

// -----------------------------
// Types for GetTransactionStatus
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetTransactionStatusRequest {
    /// Transaction hash ID (with or without 0x prefix)
    pub tx_hash: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusResult {
    /// Transaction hash ID
    pub id: String,
    /// Transaction status (e.g., "pending", "confirmed", "failed")
    pub status: String,
}

// -----------------------------
// Types for GetTransactions
// -----------------------------
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
    /// Direction of involvement (e.g., "in", "out")
    pub direction: String,
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
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionsResult {
    /// List of transactions
    pub transactions: Vec<TransactionInfo>,
}

// -----------------------------
// Types for GetWalletConfig
// -----------------------------
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
}

// -----------------------------
// Types for Deposit
// -----------------------------
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

// -----------------------------
// Types for Transfer
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct TransferRequest {
    /// Value of the note to transfer
    pub value: String,
    /// Rho (nonce) of the input note to spend (hex string)
    pub input_rho: String,
    /// Recipient of the input note to spend (hex string)
    pub input_recipient: String,
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

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    provider: Option<Arc<Provider>>,
    wallet_context: Option<Arc<RwLock<McpWalletContext>>>,
    ligero_prover: Option<Arc<LigeroProver>>,
    /// Authority FVK for decrypting encrypted notes (to be used in future decrypt implementation)
    #[allow(dead_code)]
    authority_fvk: Option<Arc<AuthorityFvk>>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct WalletStatusResult {
    pub status: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WalletStatusRequest {}

#[allow(rust_analyzer::macro_error)]
#[tool_router]
impl CryptoServer {
    pub fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
        authority_fvk: Option<Arc<AuthorityFvk>>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context: Some(wallet_context),
            ligero_prover: Some(ligero_prover),
            authority_fvk,
        }
    }

    /// Send funds to another wallet address.
    /// Creates and broadcasts a transaction to send a specified amount of funds to a destination address.
    #[tool(
        name = "sendFunds",
        description = "Send funds to another wallet address. Creates and broadcasts a transaction to send a specified amount of funds to a destination address."
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

        // NOTE: destination_address is currently ignored - temporary implementation using update_value_zk
        tracing::debug!(
            "sendFunds called with destination_address: {} (currently ignored), amount: {}",
            params.destination_address,
            params.amount
        );

        let operation_result =
            crate::operations::update_value_zk(ligero, provider, &*ctx, params.amount)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = SendFundsResult {
            tx_hash: operation_result.tx_hash,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the current synchronization status of the wallet.
    #[tool(
        name = "walletStatus",
        description = "Get the current synchronization status of the wallet."
    )]
    async fn wallet_status(
        &self,
        Parameters(_params): Parameters<WalletStatusRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = WalletStatusResult {
            status: "synchronized".to_string(),
        };
        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
    /// Get the balance of the wallet's default address for a given token ID.
    /// Requires wallet context to be configured.
    #[tool(
        name = "walletBalance",
        description = "Get the balance of the wallet's default address for a given token ID."
    )]
    async fn wallet_balance(
        &self,
        Parameters(params): Parameters<GetWalletBalanceRequest>,
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

        let (address, balance) =
            crate::operations::get_default_token_balance(provider, &*ctx, &params.token_id)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetWalletBalanceResult {
            address,
            token_id: params.token_id,
            balance: balance.to_string(),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return the wallet's default address.
    #[tool(
        name = "walletAddress",
        description = "Return the wallet's default address."
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

        let result = GetWalletAddressResult { address };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the status of a transaction by its ID.
    #[tool(
        name = "getTransactionStatus",
        description = "Get the status of a transaction by its ID. Retrieves the current status of a specific transaction."
    )]
    async fn get_transaction_status(
        &self,
        Parameters(params): Parameters<GetTransactionStatusRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let tx_status = crate::operations::get_transaction_status(provider, &params.tx_hash)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetTransactionStatusResult {
            id: tx_status.id,
            status: tx_status.status,
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
                direction: tx.direction,
                sender: tx.sender,
                recipient: tx.recipient,
                amount: tx.amount,
                anchor_root: tx.anchor_root,
                nullifier: tx.nullifier,
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
        description = "Get the wallet's configuration. Retrieves the configuration of the wallet, including the RPC URL, wallet address, chain ID, and chain name."
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

        let config = crate::operations::get_wallet_config(provider, &*ctx)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetWalletConfigResult {
            rpc_url: config.rpc_url,
            address: config.address,
            chain_id: config.chain_id,
            chain_name: config.chain_name,
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

        tracing::debug!("deposit called with amount: {}", amount);

        let deposit_result = crate::operations::deposit(provider, &*ctx, amount)
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
        description = "Transfer funds within the Midnight Privacy shielded pool. Uses ZK proofs to spend a note and create a new output note."
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

        tracing::debug!(
            "transfer called with value: {}, input_rho: {}, input_recipient: {}",
            value,
            hex::encode(&input_rho),
            hex::encode(&input_recipient)
        );

        let transfer_result =
            crate::operations::transfer(ligero, provider, &*ctx, value, input_rho, input_recipient)
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
