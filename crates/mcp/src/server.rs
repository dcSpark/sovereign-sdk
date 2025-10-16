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
// Types for UpdateValueZk
// -----------------------------
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateValueZkRequest {
    /// The new value to set
    pub new_value: i64,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct UpdateValueZkResult {
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

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    provider: Option<Arc<Provider>>,
    wallet_context: Option<Arc<RwLock<McpWalletContext>>>,
    ligero_prover: Option<Arc<LigeroProver>>,
}

// Generate a ToolRouter over the tool functions in this impl block.
#[tool_router]
impl CryptoServer {
    pub fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context: Some(wallet_context),
            ligero_prover: Some(ligero_prover),
        }
    }

    /// Generate a zero-knowledge proof for updating a value.
    /// This proof demonstrates that the value update is valid without revealing private information.
    #[tool(
        name = "update_value_zk",
        description = "Generate a zero-knowledge proof for updating a value. Returns the proof data."
    )]
    async fn update_value_zk(
        &self,
        Parameters(params): Parameters<UpdateValueZkRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Check if provider is available
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        // Check if ligero prover is available
        let ligero = self.ligero_prover.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Ligero prover not configured. Please set LIGERO_PROVER_BINARY_PATH and LIGERO_SHADER_PATH environment variables.",
                None,
            )
        })?;

        // Check if wallet context is available
        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        // Lock the wallet context for reading
        let ctx = wallet_ctx.read().await;

        // TODO: Get chain_id from somewhere (config, wallet, or runtime)
        // For now, using a placeholder chain_id
        let chain_id = 4321; // Placeholder - should come from config

        // Call the core operation (contains all business logic)
        let operation_result = crate::operations::update_value_zk(
            ligero,
            provider,
            &*ctx,
            params.new_value,
            chain_id,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Return the result with just the transaction hash
        let result = UpdateValueZkResult {
            tx_hash: operation_result.tx_hash,
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
        // Check if provider is available
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        // Check if wallet context is available
        let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        // Lock the wallet context for reading
        let ctx = wallet_ctx.read().await;

        // Call the core operation
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
