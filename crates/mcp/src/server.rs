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

use demo_stf::runtime::Runtime;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::wallet::WalletContext;

// Define the concrete spec type used by the MCP server
// This matches the MockRollupSpec from rollup-ligero
pub type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;
// WalletContext takes the DispatchCall type itself, not its Decodable
pub type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

// -----------------------------
// Types for SendTransaction
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct SendTransactionRequest {
    /// Destination address (chain-specific format, e.g., EVM hex, etc.)
    pub to: String,
    /// Human-readable amount (keep String to avoid float pitfalls; parse in real impl)
    pub amount: String,
    /// Optional asset/symbol or contract address (e.g., "ETH", "USDC", "0x...").
    pub asset: Option<String>,
    /// Optional network identifier (e.g., "ethereum-mainnet", "solana", "polygon").
    pub network: Option<String>,
    /// Optional extra fields you may need (nonce, gas config, memo, etc.)
    pub nonce: Option<u64>,
    pub gas_limit: Option<u64>,
    pub gas_price: Option<String>,
    pub data: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SendTransactionResult {
    pub tx_hash: String,
    pub network: Option<String>,
    /// For UX / inspection; add fields as needed later
    pub submitted: bool,
    pub note: Option<String>,
}

// -----------------------------
// Types for GetBalance
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetBalanceRequest {
    pub address: String,
    /// Optional asset/symbol or contract address to query
    pub asset: Option<String>,
    pub network: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetBalanceResult {
    pub address: String,
    pub asset: String,
    pub balance: String,
    pub network: String,
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

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    wallet_context: Option<Arc<RwLock<McpWalletContext>>>,
}

// Generate a ToolRouter over the tool functions in this impl block.
#[tool_router]
impl CryptoServer {
    pub fn with_wallet(wallet_context: Arc<RwLock<McpWalletContext>>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            wallet_context: Some(wallet_context),
        }
    }

    /// Broadcast a transaction to the specified network.
    /// (Boilerplate only — replace the placeholder implementation.)
    #[tool(
        name = "send_transaction",
        description = "Broadcast a crypto transaction. Returns a tx hash."
    )]
    async fn send_transaction(
        &self,
        Parameters(params): Parameters<SendTransactionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // TODO: Implement signing / RPC broadcast here.
        let result = SendTransactionResult {
            tx_hash: "0xPLACEHOLDER".to_string(),
            network: params.network,
            submitted: true,
            note: Some("Not yet implemented".to_string()),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return a balance for an address / asset on a network.
    /// (Boilerplate only — replace the placeholder implementation.)
    #[tool(
        name = "get_balance",
        description = "Get the balance for an address (optionally for a specific asset)."
    )]
    async fn get_balance(
        &self,
        Parameters(params): Parameters<GetBalanceRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = GetBalanceResult {
            address: params.address,
            asset: params.asset.unwrap_or_else(|| "ETH".to_string()),
            balance: "0".to_string(),
            network: params
                .network
                .unwrap_or_else(|| "ethereum-mainnet".to_string()),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return the wallet's default address.
    #[tool(
        name = "get_wallet_address",
        description = "Return the wallet's default address."
    )]
    async fn get_wallet_address(
        &self,
        Parameters(_params): Parameters<GetWalletAddressRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Check if wallet context is available
        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH and ROLLUP_RPC_URL environment variables.",
                None,
            )
        })?;

        // Lock the wallet context for reading
        let ctx = wallet_ctx.read().await;

        // Call the core operation
        let address = crate::operations::get_default_address(&*ctx)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = GetWalletAddressResult { address };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the balance of the wallet's default address for a given token ID.
    /// Requires wallet context to be configured.
    #[tool(
        name = "get_wallet_balance",
        description = "Get the balance of the wallet's default address for a given token ID."
    )]
    async fn get_wallet_balance(
        &self,
        Parameters(params): Parameters<GetWalletBalanceRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Check if wallet context is available
        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH and ROLLUP_RPC_URL environment variables.",
                None,
            )
        })?;

        // Lock the wallet context for reading
        let ctx = wallet_ctx.read().await;

        // Call the core operation
        let (address, balance) =
            crate::operations::get_default_token_balance(&*ctx, &params.token_id)
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
}

// Generate `list_tools`/`call_tool` by delegating to the router above,
// and provide basic server info/capabilities.
#[tool_handler]
impl ServerHandler for CryptoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            // Optional instructions that MCP clients can show the model/agent
            instructions: Some(
                "Sovereign SDK MCP Server: Tools for querying wallet balances and interacting with Sovereign rollups.".into(),
            ),
            // Expose tool capability (resources/prompts can be added later)
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
