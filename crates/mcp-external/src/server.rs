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
use sov_bank::config_gas_token_id;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::{Amount, Spec};
use tokio::sync::RwLock;
use url::Url;

use crate::fvk_service::{fetch_viewer_fvk_bundle, parse_hex_32, ViewerFvkBundle};
use crate::ligero::Ligero as LigeroProver;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;
pub type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];

async fn run_auto_fund_sequence(
    provider: Arc<Provider>,
    admin_ctx: Arc<McpWalletContext>,
    dest_wallet_address: String,
    dest_privacy_key: PrivacyKey,
    new_wallet_for_deposit: Arc<McpWalletContext>,
    deposit_amount: u128,
    auto_fund_gas_reserve: u128,
) {
    // Total L2 funding: deposit amount + extra for gas fees
    let min_gas_reserve = crate::operations::DEFAULT_MAX_FEE;
    let gas_reserve = if auto_fund_gas_reserve < min_gas_reserve {
        tracing::warn!(
            "[auto-fund/createWallet] AUTO_FUND_GAS_RESERVE {} is below min {}, using {}",
            auto_fund_gas_reserve,
            min_gas_reserve,
            min_gas_reserve
        );
        min_gas_reserve
    } else {
        auto_fund_gas_reserve
    };
    let l2_funding_amount = deposit_amount + gas_reserve;

    tracing::info!(
        "[auto-fund/createWallet] Starting funding sequence: {} L2 tokens to wallet (deposit {} + gas reserve {}), then {} deposit to privacy pool",
        l2_funding_amount,
        deposit_amount,
        gas_reserve,
        deposit_amount
    );

    let gas_token_id = match provider.get_gas_token_id().await {
        Ok(token_id) => {
            let configured_id = config_gas_token_id();
            if token_id != configured_id {
                tracing::warn!(
                    "[auto-fund/createWallet] Gas token mismatch: chain {}, configured {}",
                    token_id,
                    configured_id
                );
            }
            token_id
        }
        Err(e) => {
            tracing::warn!(
                "[auto-fund/createWallet] Failed to fetch gas token id from rollup: {}. Falling back to configured gas token.",
                e
            );
            config_gas_token_id()
        }
    };

    // Step 1: Admin sends L2 tokens to the new wallet
    tracing::info!(
        "[auto-fund/createWallet] Step 1: Admin sending {} L2 tokens to {}",
        l2_funding_amount,
        dest_wallet_address
    );
    match crate::operations::send_funds(
        &provider,
        &admin_ctx,
        &dest_wallet_address,
        &gas_token_id,
        Amount::from(l2_funding_amount),
    )
    .await
    {
        Ok(res) => {
            tracing::info!(
                "[auto-fund/createWallet] Step 1 complete: L2 funding tx {}",
                res.tx_hash
            );
            let tx_hash = match res.tx_hash.parse() {
                Ok(hash) => hash,
                Err(e) => {
                    tracing::warn!(
                        "[auto-fund/createWallet] Failed to parse L2 funding tx hash {}: {}. Skipping Step 2.",
                        res.tx_hash,
                        e
                    );
                    return;
                }
            };
            if let Err(e) = provider.wait_for_tx_processing(&tx_hash).await {
                tracing::warn!(
                    "[auto-fund/createWallet] Failed waiting for L2 funding tx processing: {}. Skipping Step 2.",
                    e
                );
                return;
            }

            let dest_wallet_address_parsed: <McpSpec as Spec>::Address =
                match dest_wallet_address.parse() {
                    Ok(address) => address,
                    Err(e) => {
                        tracing::warn!(
                            "[auto-fund/createWallet] Invalid L2 wallet address '{}': {}. Skipping Step 2.",
                            dest_wallet_address,
                            e
                        );
                        return;
                    }
                };

            let max_wait = std::time::Duration::from_secs(30);
            let poll_interval = std::time::Duration::from_secs(2);
            let started = std::time::Instant::now();

            loop {
                match provider
                    .get_balance::<McpSpec>(&dest_wallet_address_parsed, &gas_token_id)
                    .await
                {
                    Ok(balance) => {
                        let balance_u128: u128 = balance.0;
                        if balance_u128 >= l2_funding_amount {
                            tracing::info!(
                                "[auto-fund/createWallet] L2 funding confirmed: {}",
                                balance_u128
                            );
                            break;
                        }

                        tracing::info!(
                            "[auto-fund/createWallet] Waiting for L2 funding: {} / {}",
                            balance_u128,
                            l2_funding_amount
                        );
                    }
                    Err(e) => tracing::warn!(
                        "[auto-fund/createWallet] Failed to query L2 balance while waiting for funding: {}",
                        e
                    ),
                }

                if started.elapsed() >= max_wait {
                    tracing::warn!(
                        "[auto-fund/createWallet] Timed out waiting for L2 funding; skipping privacy deposit"
                    );
                    return;
                }

                tokio::time::sleep(poll_interval).await;
            }

            // Step 2: New wallet deposits to privacy pool
            tracing::info!(
                "[auto-fund/createWallet] Step 2: New wallet {} depositing {} to privacy pool",
                dest_wallet_address,
                deposit_amount
            );
            match crate::operations::deposit(
                &provider,
                &new_wallet_for_deposit,
                deposit_amount,
                &dest_privacy_key,
            )
            .await
            {
                Ok(res) => tracing::info!(
                    "[auto-fund/createWallet] Step 2 complete: Privacy pool deposit tx {}",
                    res.tx_hash
                ),
                Err(e) => {
                    tracing::warn!(
                        "[auto-fund/createWallet] Step 2 failed (privacy pool deposit): {}",
                        e
                    )
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "[auto-fund/createWallet] Step 1 failed (L2 funding): {}. Skipping Step 2.",
                e
            )
        }
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SendFundsRequest {
    #[serde(rename = "destinationAddress")]
    pub destination_address: String,
    pub amount: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SendFundsResult {
    /// Transaction hash from the rollup.
    pub id: String,
    /// Current transaction state (always "initiated").
    pub state: String,
    /// Recipient wallet address.
    #[serde(rename = "toAddress")]
    pub to_address: String,
    /// Amount sent.
    pub amount: String,
    /// Timestamp (ms since epoch) when the transaction was created.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletAddressRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletAddressResult {
    /// Privacy pool address for receiving shielded funds
    pub address: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletBalanceRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletBalanceResult {
    /// The total spendable balance in the wallet (privacy pool balance)
    pub balance: String,
    /// Coins that are pending and not yet available for spending
    #[serde(rename = "pendingBalance")]
    pub pending_balance: String,
}

// Types for VerifyTransaction
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct VerifyTransactionRequest {
    /// Transaction hash identifier
    pub identifier: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VerifyTransactionLag {
    #[serde(rename = "applyGap")]
    pub apply_gap: String,
    #[serde(rename = "sourceGap")]
    pub source_gap: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VerifyTransactionSyncStatus {
    #[serde(rename = "syncedIndices")]
    pub synced_indices: String,
    pub lag: VerifyTransactionLag,
    #[serde(rename = "isFullySynced")]
    pub is_fully_synced: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VerifyTransactionResult {
    /// Whether the transaction exists in the wallet
    pub exists: bool,
    #[serde(rename = "syncStatus")]
    pub sync_status: VerifyTransactionSyncStatus,
    /// Amount of the transaction (if known)
    #[serde(rename = "transactionAmount")]
    pub transaction_amount: String,
}

// Types for GetTransactionStatus
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetTransactionStatusRequest {
    /// Transaction hash identifier
    #[serde(rename = "transactionId")]
    pub transaction_id: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusRecord {
    /// Transaction hash
    pub id: String,
    /// Current transaction state
    pub state: String,
    /// Sender address
    #[serde(rename = "fromAddress")]
    pub from_address: String,
    /// Recipient address
    #[serde(rename = "toAddress")]
    pub to_address: String,
    /// Amount in dust format
    pub amount: String,
    /// Transaction identifier (when available)
    #[serde(rename = "txIdentifier", skip_serializing_if = "Option::is_none")]
    pub tx_identifier: Option<String>,
    /// Timestamp of creation
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Timestamp of last update
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    /// Error message if transaction failed
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusLag {
    #[serde(rename = "applyGap")]
    pub apply_gap: String,
    #[serde(rename = "sourceGap")]
    pub source_gap: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusSyncStatus {
    #[serde(rename = "syncedIndices")]
    pub synced_indices: String,
    pub lag: GetTransactionStatusLag,
    #[serde(rename = "isFullySynced")]
    pub is_fully_synced: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusBlockchainStatus {
    pub exists: bool,
    #[serde(rename = "syncStatus")]
    pub sync_status: GetTransactionStatusSyncStatus,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusResult {
    pub transaction: GetTransactionStatusRecord,
    #[serde(rename = "blockchainStatus", skip_serializing_if = "Option::is_none")]
    pub blockchain_status: Option<GetTransactionStatusBlockchainStatus>,
}

// Types for GetTransactions
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetTransactionsRequest {}

type GetTransactionsRecord = GetTransactionStatusRecord;

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct GetTransactionsResult(pub Vec<GetTransactionsRecord>);

// Types for GetWalletConfig
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletConfigRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletConfigResult {
    /// The URL of the Indexer service.
    pub indexer: String,
    /// The WebSocket URL of the Indexer service.
    #[serde(rename = "indexerWS")]
    pub indexer_ws: String,
    /// The URL of the Midnight node.
    pub node: String,
    /// The URL of the proof server.
    pub proof_server: String,
    /// Directory for log files.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "logDir")]
    pub log_dir: Option<String>,
    /// The network identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "networkId")]
    pub network_id: Option<String>,
    /// Flag indicating if an external proof server is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "useExternalProofServer")]
    pub use_external_proof_server: Option<bool>,
}

// Types for Deposit
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct DepositRequest {
    /// Amount to deposit into the shielded pool
    pub amount: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct TransferRequest {
    /// Transaction hash of the note to spend (from unspent_notes in walletBalance)
    #[allow(dead_code)]
    pub note_tx_hash: String,
    /// Destination privacy address (bech32 format: privpool1...)
    #[allow(dead_code)]
    pub destination_address: String,
    /// Amount to send. If less than note value, change is returned to your privacy address.
    /// If not provided, sends the full note value.
    #[serde(default)]
    #[allow(dead_code)]
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
    #[serde(rename = "privacyAddress")]
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
    #[serde(rename = "privacyAddress")]
    pub privacy_address: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct UnfreezeAddressResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListFrozenAddressesRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ListFrozenAddressesResult {
    /// Frozen privacy pool addresses (bech32m format: privpool1...)
    pub addresses: Vec<String>,
    /// Total count.
    pub count: u64,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddPoolAdminRequest {
    /// L2 address to grant pool-admin rights to
    #[serde(rename = "adminAddress")]
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
    #[serde(rename = "adminAddress")]
    pub admin_address: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RemovePoolAdminResult {
    /// Transaction hash from the rollup
    pub tx_hash: String,
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
    /// Sender identifier bound into NOTE_V2 commitments for transfer notes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
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

// Types for RemoveWallet
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RemoveWalletRequest {}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RemoveWalletResult {
    /// Whether the wallet was successfully removed
    pub success: bool,
    /// Message describing the result
    pub message: String,
}

// Types for WalletStatus
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetWalletStatusRequest {}

/// Sync progress information
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SyncProgressInfo {
    /// Whether the wallet is fully synced
    pub synced: bool,
    /// Lag information
    pub lag: LagInfoData,
    /// Sync percentage (0-100)
    pub percentage: f64,
}

/// Lag information for sync status
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LagInfoData {
    /// Apply gap value
    #[serde(rename = "applyGap")]
    pub apply_gap: String,
    /// Source gap value
    #[serde(rename = "sourceGap")]
    pub source_gap: String,
}

/// Wallet balances information
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct BalancesInfo {
    /// Available spendable funds
    pub balance: String,
    /// Funds not yet available for spending
    #[serde(rename = "pendingBalance")]
    pub pending_balance: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetWalletStatusResult {
    /// Whether the wallet is ready for operations
    pub ready: bool,
    /// Whether the wallet is currently syncing
    pub syncing: bool,
    /// Sync progress information
    #[serde(rename = "syncProgress")]
    pub sync_progress: SyncProgressInfo,
    /// The wallet's privacy pool address
    pub address: String,
    /// Current wallet balances
    pub balances: BalancesInfo,
    /// Whether the wallet is in recovery mode
    pub recovering: bool,
    /// Number of recovery attempts made
    #[serde(rename = "recoveryAttempts")]
    pub recovery_attempts: u32,
    /// Maximum number of recovery attempts allowed
    #[serde(rename = "maxRecoveryAttempts")]
    pub max_recovery_attempts: u32,
    /// Whether the wallet is fully synced
    #[serde(rename = "isFullySynced")]
    pub is_fully_synced: bool,
}

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    provider: Option<Arc<Provider>>,
    wallet_context: Option<Arc<RwLock<McpWalletContext>>>,
    admin_wallet_context: Option<Arc<McpWalletContext>>,
    ligero_prover: Option<Arc<LigeroProver>>,
    viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
    privacy_key: Arc<RwLock<PrivacyKey>>,
    log_path: String,
    auto_fund_deposit_amount: Option<u128>,
    auto_fund_gas_reserve: u128,
    /// Tracks whether a wallet has been explicitly loaded via createWallet or restoreWallet.
    /// When true, createWallet and restoreWallet will fail until removeWallet is called.
    wallet_explicitly_loaded: Arc<RwLock<bool>>,
}

#[allow(rust_analyzer::macro_error)]
#[tool_router]
impl CryptoServer {
    pub fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<McpWalletContext>>,
        admin_wallet_context: Option<Arc<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
        viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
        privacy_key: Arc<RwLock<PrivacyKey>>,
        log_path: String,
        auto_fund_deposit_amount: Option<u128>,
        auto_fund_gas_reserve: u128,
        wallet_explicitly_loaded: Arc<RwLock<bool>>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context: Some(wallet_context),
            admin_wallet_context,
            ligero_prover: Some(ligero_prover),
            viewer_fvk_bundle,
            privacy_key,
            log_path,
            auto_fund_deposit_amount,
            auto_fund_gas_reserve,
            wallet_explicitly_loaded,
        }
    }

    /// Send funds from the privacy pool using the first available unspent note.
    #[tool(
        name = "send",
        description = "Send funds from the privacy pool to a destination privacy address. Uses the first unspent note and submits a privacy transfer."
    )]
    async fn send_funds(
        &self,
        Parameters(params): Parameters<SendFundsRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        use midnight_privacy::{recipient_from_pk_v2, PrivacyAddress};
        const DOMAIN: [u8; 32] = [1u8; 32];

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

        let viewer_fvk_bundle_for_transfer = self.viewer_fvk_bundle.read().await.clone();
        let viewer_fvk_bytes = viewer_fvk_bundle_for_transfer
            .as_ref()
            .map(|bundle| bundle.fvk)
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    "Viewer key not configured. Set POOL_FVK_PK and ensure midnight-fvk-service is running.",
                    None,
                )
            })?;

        let privacy_key_guard = self.privacy_key.read().await;
        let output_privacy_addr: PrivacyAddress = params.destination_address.parse().map_err(|e| {
            ErrorData::invalid_params(
                format!(
                    "Invalid destinationAddress format. Must be a privacy address (privpool1...): {}",
                    e
                ),
                None,
            )
        })?;

        let output_pk = output_privacy_addr.to_pk();
        let output_pk_ivk = output_privacy_addr.pk_ivk();
        let viewing_key = midnight_privacy::FullViewingKey(viewer_fvk_bytes);

        let send_amount = params.amount.parse::<u128>().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid amount format: {}", e), None)
        })?;

        if send_amount == 0 {
            return Err(ErrorData::invalid_params(
                "Amount must be greater than 0".to_string(),
                None,
            ));
        }

        let ctx_guard = wallet_ctx.read().await;

        let privacy_result = crate::operations::get_privacy_balance(
            provider,
            &*privacy_key_guard,
            Some(&viewing_key),
        )
        .await
        .map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch unspent notes: {}", e), None)
        })?;

        if privacy_result.unspent_notes.is_empty() {
            return Err(ErrorData::invalid_params(
                "No unspent notes available to send.".to_string(),
                None,
            ));
        }

        // Smart note selection: choose the best note based on the amount
        // Strategy:
        // 1. Look for exact match (no change needed)
        // 2. If no exact match, find smallest note >= send_amount (minimize change)
        // 3. If no note is large enough, fail with insufficient funds error
        let note = {
            let mut exact_match = None;
            let mut smallest_sufficient = None;
            let mut smallest_sufficient_value = u128::MAX;

            for n in &privacy_result.unspent_notes {
                if n.value == send_amount {
                    // Perfect match found
                    exact_match = Some(n);
                    break;
                } else if n.value > send_amount && n.value < smallest_sufficient_value {
                    // Track smallest note that's larger than needed
                    smallest_sufficient = Some(n);
                    smallest_sufficient_value = n.value;
                }
            }

            match exact_match.or(smallest_sufficient) {
                Some(n) => n,
                None => {
                    let total_balance: u128 =
                        privacy_result.unspent_notes.iter().map(|n| n.value).sum();
                    return Err(ErrorData::invalid_params(
                        format!(
                            "Insufficient funds: trying to send {} but no single note is large enough. Total balance: {}, available notes: {}",
                            send_amount,
                            total_balance,
                            privacy_result.unspent_notes.len()
                        ),
                        None,
                    ));
                }
            }
        };

        tracing::info!(
            "[send] Selected note - value: {}, tx_hash: {}, strategy: {}",
            note.value,
            note.tx_hash,
            if note.value == send_amount {
                "exact match"
            } else {
                "smallest sufficient"
            }
        );

        let rho_bytes = match hex::decode(note.rho.trim_start_matches("0x")) {
            Ok(bytes) if bytes.len() == 32 => bytes,
            Ok(bytes) => {
                return Err(ErrorData::internal_error(
                    format!("Invalid rho length ({} bytes)", bytes.len()),
                    None,
                ));
            }
            Err(e) => {
                return Err(ErrorData::internal_error(
                    format!("Failed to decode rho: {}", e),
                    None,
                ));
            }
        };

        let mut input_rho = [0u8; 32];
        input_rho.copy_from_slice(&rho_bytes);
        let input_recipient = privacy_key_guard.recipient(&DOMAIN);
        let input_sender_id: [u8; 32] = if let Some(sender_id_hex) = note.sender_id.as_deref() {
            let bytes = hex::decode(sender_id_hex.trim_start_matches("0x")).map_err(|e| {
                ErrorData::internal_error(
                    format!("Invalid sender_id in note (hex decode failed): {}", e),
                    None,
                )
            })?;
            if bytes.len() != 32 {
                return Err(ErrorData::internal_error(
                    format!(
                        "Invalid sender_id length in note (expected 32 bytes, got {})",
                        bytes.len()
                    ),
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
        let output_recipient = recipient_from_pk_v2(&DOMAIN, &output_pk, &output_pk_ivk);

        tracing::info!(
            "[send] Input note - value: {}, rho: {}, recipient: {}",
            note.value,
            hex::encode(&input_rho),
            hex::encode(&input_recipient)
        );
        tracing::info!(
            "[send] Output recipient (destination): {}",
            hex::encode(&output_recipient)
        );

        if send_amount < note.value {
            let change_amt = note.value - send_amount;
            tracing::info!(
                "[send] Transfer includes change output - amount: {}",
                change_amt
            );
        } else {
            tracing::info!("[send] No change needed - sending full note value");
        }

        let spend_sk = privacy_key_guard.spend_sk().copied().ok_or_else(|| {
            ErrorData::internal_error(
                "privacy key missing spend_sk; cannot spend note".to_string(),
                None,
            )
        })?;
        let pk_ivk_owner = privacy_key_guard.pk_ivk(&DOMAIN);
        let ligero_ref = self.ligero_prover.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Ligero proof service not configured; set LIGERO_PROOF_SERVICE_URL.".to_string(),
                None,
            )
        })?;
        let transfer_result = crate::operations::transfer(
            ligero_ref,
            provider,
            &*ctx_guard,
            spend_sk,
            pk_ivk_owner,
            note.value,
            send_amount,
            input_rho,
            input_sender_id,
            output_pk,
            output_pk_ivk,
            viewer_fvk_bundle_for_transfer,
        )
        .await
        .map_err(|e| {
            ErrorData::internal_error(format!("Failed to submit privacy transfer: {}", e), None)
        })?;

        let created_at = transfer_result.created_at;

        let result = SendFundsResult {
            id: transfer_result.tx_hash,
            state: "initiated".to_string(),
            to_address: output_privacy_addr.to_string(),
            amount: send_amount.to_string(),
            created_at,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the current balance of the wallet.
    /// Fetches the current balance from the privacy pool.
    #[tool(
        name = "walletBalance",
        description = "Get the current balance of the wallet. Fetches the current balance of the wallet from the Midnight network."
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

        let _wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Wallet context not configured. Please set WALLET_PATH environment variable.",
                None,
            )
        })?;

        let viewer_fvk_guard = self.viewer_fvk_bundle.read().await;
        let viewing_key = viewer_fvk_guard
            .as_ref()
            .map(|bundle| midnight_privacy::FullViewingKey(bundle.fvk));

        let privacy_key_guard = self.privacy_key.read().await;

        let privacy_result = crate::operations::get_privacy_balance(
            provider,
            &*privacy_key_guard,
            viewing_key.as_ref(),
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let balance = privacy_result.balance;

        let result = GetWalletBalanceResult {
            balance: balance.to_string(),
            pending_balance: "0".to_string(),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return the wallet's privacy pool address.
    #[tool(
        name = "walletAddress",
        description = "Return the wallet's privacy pool address for receiving shielded funds."
    )]
    async fn wallet_address(
        &self,
        Parameters(_params): Parameters<GetWalletAddressRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let privacy_key_guard = self.privacy_key.read().await;
        let privacy_address = privacy_key_guard.privacy_address(&DOMAIN).to_string();

        let result = GetWalletAddressResult {
            address: privacy_address,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Verify if a transaction has been received.
    #[tool(
        name = "verifyTransaction",
        description = "Verify if a transaction has been received. Checks whether the transaction hash exists in the wallet."
    )]
    async fn verify_transaction(
        &self,
        Parameters(params): Parameters<VerifyTransactionRequest>,
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

        let ctx = wallet_ctx.read().await;
        let privacy_key_guard = self.privacy_key.read().await;

        let transactions = crate::operations::get_transactions(provider, &*ctx, &*privacy_key_guard)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let normalize = |value: &str| value.trim().trim_start_matches("0x").to_ascii_lowercase();
        let target = normalize(&params.identifier);

        let mut exists = false;
        let mut transaction_amount = "0".to_string();
        if let Some(tx) = transactions
            .into_iter()
            .find(|tx| normalize(&tx.tx_hash) == target)
        {
            exists = true;
            if let Some(amount) = tx.amount {
                transaction_amount = amount;
            }
        }

        let result = VerifyTransactionResult {
            exists,
            sync_status: VerifyTransactionSyncStatus {
                synced_indices: "".to_string(),
                lag: VerifyTransactionLag {
                    apply_gap: "".to_string(),
                    source_gap: "".to_string(),
                },
                is_fully_synced: true,
            },
            transaction_amount,
        };

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
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

        let tx_option = provider
            .get_transaction(&params.transaction_id)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let tx = tx_option.ok_or_else(|| {
            ErrorData::invalid_params(
                "Transaction not found for this wallet.".to_string(),
                None,
            )
        })?;

        let transaction = build_transaction_record(
            tx.tx_hash.clone(),
            tx.status.clone(),
            tx.privacy_sender.clone(),
            tx.sender.clone(),
            tx.privacy_recipient.clone(),
            tx.recipient.clone(),
            tx.amount.clone(),
            tx.timestamp_ms,
        );

        let result = GetTransactionStatusResult {
            transaction,
            blockchain_status: Some(GetTransactionStatusBlockchainStatus {
                exists: true,
                sync_status: GetTransactionStatusSyncStatus {
                    synced_indices: "".to_string(),
                    lag: GetTransactionStatusLag {
                        apply_gap: "".to_string(),
                        source_gap: "".to_string(),
                    },
                    is_fully_synced: true,
                },
            }),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get all transactions for the privacy pool.
    /// Returns transactions from the indexer filtered to the current privacy pool address.
    #[tool(
        name = "getTransactions",
        description = "Get all transactions for the privacy pool. Retrieves all transactions (deposits, transfers, withdrawals) associated with the current privacy pool address."
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

        let transactions = crate::operations::get_transactions(provider, &*ctx, &*privacy_key_guard)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let transaction_records: Vec<GetTransactionsRecord> = transactions
            .into_iter()
            .map(|tx| {
                build_transaction_record(
                    tx.tx_hash,
                    tx.status,
                    tx.privacy_sender,
                    tx.sender,
                    tx.privacy_recipient,
                    tx.recipient,
                    tx.amount,
                    tx.timestamp_ms,
                )
            })
            .collect();

        let result = GetTransactionsResult(transaction_records);

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "[]".to_string());

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

        let chain_data = provider
            .get_chain_data()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let indexer = provider.indexer_url().to_string();
        let indexer_ws = "undefined".to_string();
        let node = provider.rpc_url().to_string();

        let (proof_server, use_external_proof_server) = match self.ligero_prover.as_ref() {
            Some(prover) => (prover.proof_service_url().to_string(), Some(true)),
            None => ("".to_string(), Some(false)),
        };

        let result = GetWalletConfigResult {
            indexer,
            indexer_ws,
            node,
            proof_server,
            log_dir: Some(self.log_path.clone()),
            network_id: Some(chain_data.chain_name),
            use_external_proof_server,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // deposit tool removed; funding is attempted on startup when configured via env

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

        // Check if a wallet is already loaded
        let is_loaded = *self.wallet_explicitly_loaded.read().await;
        if is_loaded {
            return Err(ErrorData::invalid_params(
                "A wallet is already loaded. Call removeWallet first before creating a new wallet.",
                None,
            ));
        }

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

        let wallet_address_str = new_wallet_ctx.get_address().to_string();

        let new_wallet_for_deposit = Arc::new(new_wallet_ctx.clone());

        // Create new privacy key
        let new_privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex).map_err(|e| {
            ErrorData::internal_error(format!("Failed to create privacy key: {}", e), None)
        })?;
        let new_privacy_key_for_deposit = new_privacy_key.clone();

        let privacy_address = new_privacy_key.privacy_address(&DOMAIN).to_string();

        // Ensure provider is configured before we swap wallet context.
        let _ = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

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

        // Replace the wallet context and privacy keys
        if let Some(ref wallet_ctx) = self.wallet_context {
            let mut ctx_guard = wallet_ctx.write().await;
            *ctx_guard = new_wallet_ctx;
        }

        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        *viewer_fvk_guard = viewer_fvk_bundle.clone();

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = new_privacy_key;

        // Mark the wallet as explicitly loaded
        let mut loaded_guard = self.wallet_explicitly_loaded.write().await;
        *loaded_guard = true;

        tracing::info!("[createWallet] New wallet created successfully");
        tracing::info!("[createWallet] Wallet address: {}", wallet_address_str);
        tracing::info!("[createWallet] Privacy address: {}", privacy_address);

        // Best-effort funding when configured via AUTO_FUND_DEPOSIT_AMOUNT
        // Flow: Admin sends L2 tokens to new wallet, then new wallet deposits to privacy pool
        if let Some(deposit_amount) = self.auto_fund_deposit_amount {
            let admin_wallet_ctx = self.admin_wallet_context.clone();
            if let (Some(provider), Some(admin_ctx)) = (self.provider.clone(), admin_wallet_ctx) {
                let dest_privacy_key = new_privacy_key_for_deposit.clone();
                let dest_wallet_address = wallet_address_str.clone();
                let new_wallet_for_deposit = new_wallet_for_deposit.clone();
                let auto_fund_gas_reserve = self.auto_fund_gas_reserve;

                tokio::spawn(async move {
                    run_auto_fund_sequence(
                        provider,
                        admin_ctx,
                        dest_wallet_address,
                        dest_privacy_key,
                        new_wallet_for_deposit,
                        deposit_amount,
                        auto_fund_gas_reserve,
                    )
                    .await;
                });
            } else {
                tracing::warn!(
                    "[auto-fund/createWallet] Auto-fund configured but ADMIN_WALLET_PRIVATE_KEY is not set; skipping"
                );
            }
        }

        let result = CreateWalletResult {
            wallet_private_key: wallet_private_key_hex,
            wallet_address: wallet_address_str,
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
    /// The L2 wallet context IS updated and will be funded with gas tokens from the admin wallet.
    /// All subsequent operations will use the restored keys.
    #[tool(
        name = "restoreWallet",
        description = "Restore a wallet from existing keys. Loads wallet private key and privacy pool spending key from hex strings. If POOL_FVK_PK is set, fetches a fresh viewer FVK from midnight-fvk-service. All subsequent operations will use the restored keys."
    )]
    async fn restore_wallet(
        &self,
        Parameters(params): Parameters<RestoreWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Check if a wallet is already loaded
        let is_loaded = *self.wallet_explicitly_loaded.read().await;
        if is_loaded {
            return Err(ErrorData::invalid_params(
                "A wallet is already loaded. Call removeWallet first before restoring a different wallet.",
                None,
            ));
        }

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

        // Ensure provider is configured before we swap wallet context.
        let _ = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

        // Replace the existing keys with the restored ones (including wallet context)
        if let Some(ref wallet_ctx) = self.wallet_context {
            let mut ctx_guard = wallet_ctx.write().await;
            *ctx_guard = new_wallet_ctx;
        }

        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        *viewer_fvk_guard = viewer_fvk_bundle;

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = new_privacy_key;

        // Mark the wallet as explicitly loaded
        let mut loaded_guard = self.wallet_explicitly_loaded.write().await;
        *loaded_guard = true;

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

    /// Remove the currently loaded wallet.
    /// Clears the wallet state so that createWallet or restoreWallet can be called again.
    /// This prevents accidental overwrites of a loaded wallet.
    #[tool(
        name = "removeWallet",
        description = "Remove the currently loaded wallet. This clears the wallet state so that createWallet or restoreWallet can be called again. Use this to safely switch wallets without accidentally overwriting an existing one."
    )]
    async fn remove_wallet(
        &self,
        Parameters(_params): Parameters<RemoveWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Check if a wallet is currently loaded
        let is_loaded = *self.wallet_explicitly_loaded.read().await;
        if !is_loaded {
            return Err(ErrorData::invalid_params(
                "No wallet is currently loaded. Use createWallet or restoreWallet first.",
                None,
            ));
        }

        // Clear the wallet_explicitly_loaded flag
        let mut loaded_guard = self.wallet_explicitly_loaded.write().await;
        *loaded_guard = false;

        tracing::info!("[removeWallet] Wallet removed successfully. createWallet and restoreWallet are now available.");

        let result = RemoveWalletResult {
            success: true,
            message: "Wallet removed successfully. You can now use createWallet or restoreWallet.".to_string(),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the current synchronization status of the wallet.
    /// Checks if the wallet is synced with the blockchain and provides sync progress and balance information.
    #[tool(
        name = "walletStatus",
        description = "Get the current synchronization status of the wallet. Checks if the wallet is synced with the Midnight blockchain."
    )]
    async fn wallet_status(
        &self,
        Parameters(_params): Parameters<GetWalletStatusRequest>,
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

        let _ctx = wallet_ctx.read().await;
        let privacy_key_guard = self.privacy_key.read().await;

        // Get the current privacy balance to include in status
        let viewer_fvk_guard = self.viewer_fvk_bundle.read().await;
        let viewing_key = viewer_fvk_guard
            .as_ref()
            .map(|bundle| midnight_privacy::FullViewingKey(bundle.fvk));

        let privacy_balance = match crate::operations::get_privacy_balance(
            provider,
            &*privacy_key_guard,
            viewing_key.as_ref(),
        )
        .await
        {
            Ok(privacy_result) => privacy_result.balance,
            Err(e) => {
                tracing::warn!("[walletStatus] Failed to fetch privacy balance: {}", e);
                0
            }
        };

        let sync_progress = SyncProgressInfo {
            synced: true,
            lag: LagInfoData {
                apply_gap: "0".to_string(),
                source_gap: "0".to_string(),
            },
            percentage: 100.0,
        };

        let result = GetWalletStatusResult {
            ready: true,
            syncing: false,
            sync_progress,
            address: privacy_key_guard.privacy_address(&DOMAIN).to_string(),
            balances: BalancesInfo {
                balance: privacy_balance.to_string(),
                pending_balance: "0".to_string(),
            },
            recovering: false,
            recovery_attempts: 0,
            max_recovery_attempts: 0,
            is_fully_synced: true,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Freeze a privacy address (pool admin only).
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

        let json = serde_json::to_string_pretty(&FreezeAddressResult { tx_hash: res.tx_hash })
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

        let json =
            serde_json::to_string_pretty(&UnfreezeAddressResult { tx_hash: res.tx_hash })
                .unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// List frozen (blacklisted) privacy addresses.
    #[tool(
        name = "listFrozenAddresses",
        description = "List frozen (blacklisted) privacy pool addresses."
    )]
    async fn list_frozen_addresses(
        &self,
        Parameters(_params): Parameters<ListFrozenAddressesRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL environment variable.",
                None,
            )
        })?;

        let res = crate::operations::list_frozen_addresses(provider)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = ListFrozenAddressesResult {
            addresses: res
                .addresses
                .into_iter()
                .map(|addr| addr.to_string())
                .collect(),
            count: res.count,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
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

        let admin: <McpSpec as Spec>::Address = params.admin_address.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid admin address: {e}"), None)
        })?;

        let res = crate::operations::add_pool_admin(provider, &*ctx, admin)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&AddPoolAdminResult { tx_hash: res.tx_hash })
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

        let admin: <McpSpec as Spec>::Address = params.admin_address.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid admin address: {e}"), None)
        })?;

        let res = crate::operations::remove_pool_admin(provider, &*ctx, admin)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json =
            serde_json::to_string_pretty(&RemovePoolAdminResult { tx_hash: res.tx_hash })
                .unwrap_or_else(|_| "{}".to_string());
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

fn map_state_from_status(status: &str) -> &'static str {
    let normalized = status.trim().to_ascii_lowercase();
    if normalized.contains("success") {
        "completed"
    } else if normalized.contains("fail") {
        "failed"
    } else if normalized.contains("pending") || normalized.contains("submitted") {
        "sent"
    } else {
        "initiated"
    }
}

fn build_transaction_record(
    tx_hash: String,
    status: Option<String>,
    privacy_sender: Option<String>,
    sender: Option<String>,
    privacy_recipient: Option<String>,
    recipient: Option<String>,
    amount: Option<String>,
    timestamp_ms: i64,
) -> GetTransactionStatusRecord {
    let status = status.unwrap_or_else(|| "Unknown".to_string());
    let state = map_state_from_status(&status).to_string();
    let from_address = privacy_sender.or(sender).unwrap_or_default();
    let to_address = privacy_recipient.or(recipient).unwrap_or_default();
    let amount = amount.unwrap_or_else(|| "0".to_string());
    let error_message = if state == "failed" {
        Some(status.clone())
    } else {
        None
    };

    GetTransactionStatusRecord {
        id: tx_hash.clone(),
        state,
        from_address,
        to_address,
        amount,
        tx_identifier: Some(tx_hash),
        created_at: timestamp_ms,
        updated_at: timestamp_ms,
        error_message,
    }
}

#[allow(dead_code)]
fn to_ws_url(http_url: &str) -> String {
    Url::parse(http_url)
        .map(|mut url| {
            let scheme = url.scheme().to_string();
            let new_scheme = match scheme.as_str() {
                "https" => "wss",
                "http" => "ws",
                other => other,
            };
            let _ = url.set_scheme(new_scheme);
            url.to_string()
        })
        .unwrap_or_else(|_| http_url.to_string())
}
