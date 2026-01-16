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
use sov_modules_api::Spec;
use tokio::sync::RwLock;
use url::Url;
use uuid::Uuid;

use crate::authority_fvk::AuthorityFvk;
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
    #[serde(rename = "destinationAddress")]
    pub destination_address: String,
    pub amount: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SendFundsResult {
    /// Transaction hash from the rollup (available once submitted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    /// Transaction hash for this transfer (same as txIdentifier)
    pub id: String,
    /// Transaction identifier (tx_hash echoed for convenience)
    #[serde(rename = "txIdentifier")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_identifier: Option<String>,
    /// Privacy transfer hash if we spent an unspent note
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_tx_hash: Option<String>,
    /// Deterministic UUID derived from the privacy tx hash
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "noteTxIdentifier")]
    pub note_tx_identifier: Option<String>,
    /// Amount sent from the unspent note
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_amount: Option<String>,
    /// Error if we couldn't send an unspent note
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "noteError")]
    pub note_error: Option<String>,
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
    /// Available unspent notes that back the balance
    pub unspent_notes: Vec<UnspentNoteInfo>,
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
pub struct TransactionRecord {
    /// Transaction hash
    pub id: String,
    /// Current state ("initiated", "sent", "completed", or "failed")
    pub state: String,
    /// Sender address (or "encrypted" if unavailable)
    #[serde(rename = "fromAddress")]
    pub from_address: String,
    /// Recipient address (or "encrypted" if unavailable)
    #[serde(rename = "toAddress")]
    pub to_address: String,
    /// Amount in dust format (or "encrypted" if unavailable)
    pub amount: String,
    /// Transaction identifier (once available)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "txIdentifier")]
    pub tx_identifier: Option<String>,
    /// Timestamp of creation (milliseconds)
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Timestamp of last update (milliseconds)
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    /// Error message if transaction failed
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "errorMessage")]
    pub error_message: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionsResult {
    /// Array of transaction records
    pub transactions: Vec<TransactionRecord>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetTransactionStatusRequest {
    /// Transaction hash (with or without 0x prefix)
    pub id: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GetTransactionStatusResult {
    pub transaction: TransactionRecord,
}

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
    /// New authority VFK (hex string)
    pub authority_vfk: String,
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
    /// Authority FVK (hex string, with or without 0x prefix)
    pub authority_fvk: String,
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

// Types for VerifyTransaction
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct VerifyTransactionRequest {
    /// The transaction hash (tx_hash) to verify. This is the rollup transaction hash returned from send/deposit operations.
    /// Can be provided with or without the '0x' prefix.
    pub identifier: String,
}

/// Sync status information for transaction verification
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VerifySyncStatus {
    /// Indices that have been synced
    #[serde(rename = "syncedIndices")]
    pub synced_indices: String,
    /// Lag information
    pub lag: VerifyLagInfo,
    /// Whether the wallet is fully synced
    #[serde(rename = "isFullySynced")]
    pub is_fully_synced: bool,
}

/// Lag information for transaction verification
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VerifyLagInfo {
    /// Apply gap value
    #[serde(rename = "applyGap")]
    pub apply_gap: String,
    /// Source gap value
    #[serde(rename = "sourceGap")]
    pub source_gap: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VerifyTransactionResult {
    /// Whether the transaction exists in the wallet
    pub exists: bool,
    /// Current sync status information
    #[serde(rename = "syncStatus")]
    pub sync_status: VerifySyncStatus,
    /// The amount of the transaction (in dust format), or "encrypted" if cannot decrypt
    #[serde(rename = "transactionAmount")]
    pub transaction_amount: String,
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
    authority_fvk: Arc<RwLock<Option<AuthorityFvk>>>,
    privacy_key: Arc<RwLock<PrivacyKey>>,
    log_path: String,
    auto_fund_deposit_amount: Option<u128>,
}

#[allow(rust_analyzer::macro_error)]
#[tool_router]
impl CryptoServer {
    pub fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<McpWalletContext>>,
        admin_wallet_context: Option<Arc<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
        authority_fvk: Arc<RwLock<Option<AuthorityFvk>>>,
        privacy_key: Arc<RwLock<PrivacyKey>>,
        log_path: String,
        auto_fund_deposit_amount: Option<u128>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context: Some(wallet_context),
            admin_wallet_context,
            ligero_prover: Some(ligero_prover),
            authority_fvk,
            privacy_key,
            log_path,
            auto_fund_deposit_amount,
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

        let authority_fvk_bytes = {
            let guard = self.authority_fvk.read().await;
            guard.as_ref().map(|v| *v.as_bytes())
        }
        .ok_or_else(|| {
            ErrorData::invalid_params(
                "Viewing key not configured. Set AUTHORITY_FVK to send from the privacy pool.",
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
        let viewing_key = midnight_privacy::FullViewingKey(authority_fvk_bytes);
        let authority_fvk_for_transfer = Some(authority_fvk_bytes);

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
            &viewing_key,
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
                "Ligero prover not configured; cannot send privacy transfer.".to_string(),
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
            authority_fvk_for_transfer,
        )
        .await
        .map_err(|e| {
            ErrorData::internal_error(format!("Failed to submit privacy transfer: {}", e), None)
        })?;

        let id = transfer_result.tx_hash.clone();
        let result = SendFundsResult {
            id,
            tx_identifier: Some(transfer_result.tx_hash.clone()),
            tx_hash: Some(transfer_result.tx_hash),
            note_tx_hash: None,
            note_tx_identifier: None,
            note_amount: None,
            note_error: None,
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

        let authority_fvk_guard = self.authority_fvk.read().await;
        let viewing_key_bytes = if let Some(ref authority_fvk) = *authority_fvk_guard {
            *authority_fvk.as_bytes()
        } else {
            return Err(ErrorData::invalid_params(
                "Viewing key not configured. Set AUTHORITY_FVK to decrypt privacy pool notes.",
                None,
            ));
        };

        let viewing_key = midnight_privacy::FullViewingKey(viewing_key_bytes);

        let privacy_key_guard = self.privacy_key.read().await;

        let privacy_result = crate::operations::get_privacy_balance(
            provider,
            &*privacy_key_guard,
            &viewing_key,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let balance = privacy_result.balance;
        let unspent_notes = privacy_result
            .unspent_notes
            .into_iter()
            .map(|note| UnspentNoteInfo {
                value: note.value.to_string(),
                rho: note.rho,
                sender_id: note.sender_id,
                tx_hash: note.tx_hash,
                timestamp_ms: note.timestamp_ms,
                kind: note.kind,
            })
            .collect();

        let result = GetWalletBalanceResult {
            balance: balance.to_string(),
            pending_balance: "0".to_string(),
            unspent_notes,
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

        // Allow lookup by either tx_hash or a UUIDv5 derived from the tx hash (legacy helper)
        let tx_hash = if Uuid::parse_str(&params.tx_hash).is_ok() {
            let wallet_ctx = self.wallet_context.as_ref().ok_or_else(|| {
                ErrorData::invalid_params(
                    "Wallet context not configured. Please set WALLET_PATH environment variable.",
                    None,
                )
            })?;

            let ctx = wallet_ctx.read().await;
            let privacy_key_guard = self.privacy_key.read().await;
            let target_uuid = Uuid::parse_str(&params.tx_hash).map_err(|e| {
                ErrorData::invalid_params(format!("Invalid transaction ID format: {}", e), None)
            })?;

            let transactions =
                crate::operations::get_transactions(provider, &*ctx, &*privacy_key_guard)
                    .await
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

            transactions
                .into_iter()
                .find_map(|tx| {
                    let candidate = Uuid::new_v5(&Uuid::NAMESPACE_OID, tx.tx_hash.as_bytes());
                    if candidate == target_uuid {
                        Some(tx.tx_hash)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    ErrorData::invalid_params(
                        "Transaction ID not found for this wallet. Try querying by tx hash.",
                        None,
                    )
                })?
        } else {
            params.tx_hash.clone()
        };

        let tx_details = crate::operations::get_transaction_status(provider, &tx_hash)
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

    /// Get the status of a transaction by its transaction hash.
    #[tool(
        name = "getTransactionStatus",
        description = "Get the status of a transaction by its hash. Returns the latest indexed state and txIdentifier."
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

        let tx_details = crate::operations::get_transaction_status(provider, &params.id)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let record = details_to_record(tx_details);

        let result = GetTransactionStatusResult { transaction: record };

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

        let transaction_records: Vec<TransactionRecord> = transactions
            .into_iter()
            .map(transaction_to_record)
            .collect();

        let result = GetTransactionsResult {
            transactions: transaction_records,
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

        let chain_data = provider
            .get_chain_data()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let indexer = provider.indexer_url().to_string();
        let indexer_ws = "undefined".to_string();
        let node = provider.rpc_url().to_string();

        let result = GetWalletConfigResult {
            indexer,
            indexer_ws,
            node,
            proof_server: "".to_string(),
            log_dir: Some(self.log_path.clone()),
            network_id: Some(chain_data.chain_name),
            use_external_proof_server: Some(false),
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // deposit tool removed; funding is attempted on startup when configured via env

    /// Create a new wallet with new keys.
    /// Generates new wallet private key, authority FVK, and privacy pool spending key.
    /// All subsequent transactions will use the new keys.
    #[tool(
        name = "createWallet",
        description = "Create a new wallet with new keys. Generates new wallet private key, authority FVK, and privacy pool spending key. All subsequent operations will use the new keys."
    )]
    async fn create_wallet(
        &self,
        Parameters(_params): Parameters<CreateWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        use rand::RngCore;

        // Generate all random bytes first (before any async operations)
        // This ensures the RNG is dropped before any await points
        let (wallet_private_key_hex, authority_fvk_hex, privacy_spend_key_hex) = {
            let mut rng = rand::thread_rng();

            // Generate new wallet private key (32 bytes)
            let mut wallet_private_key_bytes = [0u8; 32];
            rng.fill_bytes(&mut wallet_private_key_bytes);
            let wallet_private_key_hex = hex::encode(&wallet_private_key_bytes);

            // Generate new authority FVK (32 bytes)
            let mut authority_fvk_bytes = [0u8; 32];
            rng.fill_bytes(&mut authority_fvk_bytes);
            let authority_fvk_hex = hex::encode(&authority_fvk_bytes);

            // Generate new privacy spend key (32 bytes)
            let mut privacy_spend_key_bytes = [0u8; 32];
            rng.fill_bytes(&mut privacy_spend_key_bytes);
            let privacy_spend_key_hex = hex::encode(&privacy_spend_key_bytes);

            (
                wallet_private_key_hex,
                authority_fvk_hex,
                privacy_spend_key_hex,
            )
        }; // RNG is dropped here

        // Create new wallet context from the private key
        let new_wallet_ctx = McpWalletContext::from_private_key_hex(&wallet_private_key_hex)
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to create wallet context: {}", e), None)
            })?;

        let wallet_address = new_wallet_ctx.get_address().to_string();

        // Create new authority FVK
        let new_authority_fvk = AuthorityFvk::from_hex(&authority_fvk_hex).map_err(|e| {
            ErrorData::internal_error(format!("Failed to create authority FVK: {}", e), None)
        })?;

        // Create new privacy key
        let new_privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex).map_err(|e| {
            ErrorData::internal_error(format!("Failed to create privacy key: {}", e), None)
        })?;
        let new_privacy_key_for_deposit = new_privacy_key.clone();

        let privacy_address = new_privacy_key.privacy_address(&DOMAIN).to_string();

        let provider = self.provider.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                None,
            )
        })?;

        provider
            .register_vfk(&authority_fvk_hex, Some(&privacy_address))
            .await
            .map_err(|e| {
                ErrorData::internal_error(
                    format!("Failed to register authority VFK with indexer: {}", e),
                    None,
                )
            })?;

        // Replace the privacy keys (but not the wallet context)
        let mut authority_fvk_guard = self.authority_fvk.write().await;
        *authority_fvk_guard = Some(new_authority_fvk);

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = new_privacy_key;

        tracing::info!("[createWallet] New wallet created successfully");
        tracing::info!("[createWallet] Wallet address: {}", wallet_address);
        tracing::info!("[createWallet] Privacy address: {}", privacy_address);

        // Best-effort funding when configured via AUTO_FUND_DEPOSIT_AMOUNT
        if let Some(amount) = self.auto_fund_deposit_amount {
            let admin_wallet_ctx = self.admin_wallet_context.clone();
            if let (Some(provider), Some(funding_ctx)) = (self.provider.clone(), admin_wallet_ctx) {
                let dest_privacy_key = new_privacy_key_for_deposit.clone();
                tracing::info!(
                    "[auto-fund/createWallet] Attempting auto-fund deposit of {} (best-effort) using admin wallet context",
                    amount
                );
                tokio::spawn(async move {
                    tracing::info!(
                        "[auto-fund/createWallet] Submitting deposit of {} to {}",
                        amount,
                        dest_privacy_key.privacy_address(&DOMAIN)
                    );
                    match crate::operations::deposit(
                        &provider,
                        &funding_ctx,
                        amount,
                        &dest_privacy_key,
                    )
                    .await
                    {
                        Ok(res) => tracing::info!(
                            "[auto-fund/createWallet] Deposit submitted: {}",
                            res.tx_hash
                        ),
                        Err(e) => {
                            tracing::warn!("[auto-fund/createWallet] Deposit attempt failed: {}", e)
                        }
                    }
                });
            } else {
                tracing::warn!(
                    "[auto-fund/createWallet] Auto-fund deposit configured but ADMIN_WALLET_PRIVATE_KEY is not set; skipping"
                );
            }
        }

        let result = CreateWalletResult {
            wallet_private_key: wallet_private_key_hex,
            authority_vfk: authority_fvk_hex,
            privacy_spend_key: privacy_spend_key_hex,
            privacy_address,
        };

        let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Restore a wallet from existing keys.
    /// Loads existing wallet private key, authority FVK, and privacy pool spending key.
    /// All subsequent transactions will use the restored keys.
    #[tool(
        name = "restoreWallet",
        description = "Restore a wallet from existing keys. Loads wallet private key, authority FVK, and privacy pool spending key from hex strings. All subsequent operations will use the restored keys."
    )]
    async fn restore_wallet(
        &self,
        Parameters(params): Parameters<RestoreWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Strip 0x prefix if present
        let wallet_private_key_hex = params.wallet_private_key.trim_start_matches("0x");
        let authority_fvk_hex = params.authority_fvk.trim_start_matches("0x");
        let privacy_spend_key_hex = params.privacy_spend_key.trim_start_matches("0x");

        // Validate hex strings are correct length (32 bytes = 64 hex chars)
        if wallet_private_key_hex.len() != 64 {
            return Err(ErrorData::invalid_params(
                "wallet_private_key must be exactly 32 bytes (64 hex characters).",
                None,
            ));
        }
        if authority_fvk_hex.len() != 64 {
            return Err(ErrorData::invalid_params(
                "authority_fvk must be exactly 32 bytes (64 hex characters).",
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

        // Create authority FVK
        let new_authority_fvk = AuthorityFvk::from_hex(authority_fvk_hex).map_err(|e| {
            ErrorData::internal_error(format!("Failed to create authority FVK: {}", e), None)
        })?;

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

        let mut authority_fvk_guard = self.authority_fvk.write().await;
        *authority_fvk_guard = Some(new_authority_fvk);

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
        let authority_fvk_guard = self.authority_fvk.read().await;
        let privacy_balance = if let Some(ref authority_fvk) = *authority_fvk_guard {
            let viewing_key_bytes = *authority_fvk.as_bytes();
            let viewing_key = midnight_privacy::FullViewingKey(viewing_key_bytes);

            // Get privacy balance
            let privacy_result = crate::operations::get_privacy_balance(
                provider,
                &*privacy_key_guard,
                &viewing_key,
            )
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

            privacy_result.balance
        } else {
            // No viewing key configured, default to 0
            0
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

    /// Verify if a transaction has been received.
    /// Verifies the status of a transaction and attempts to decrypt it to extract the amount.
    #[tool(
        name = "verifyTransaction",
        description = "Verify if a transaction has been received. Takes a transaction hash (tx_hash) and checks if it exists in the indexer. Attempts to decrypt the transaction to extract the amount if encrypted notes are present. Use this to confirm if a payment has been received."
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

        // Get FVK if available for decryption
        let authority_fvk_guard = self.authority_fvk.read().await;
        let fvk_hex = if let Some(ref authority_fvk) = *authority_fvk_guard {
            Some(hex::encode(authority_fvk.as_bytes()))
        } else {
            None
        };

        let verify_result =
            crate::operations::verify_transaction(provider, &params.identifier, fvk_hex.as_deref())
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let result = VerifyTransactionResult {
            exists: verify_result.exists,
            sync_status: VerifySyncStatus {
                synced_indices: "all".to_string(),
                lag: VerifyLagInfo {
                    apply_gap: "0".to_string(),
                    source_gap: "0".to_string(),
                },
                is_fully_synced: true,
            },
            transaction_amount: verify_result.transaction_amount,
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

fn map_state_and_error(status: Option<String>) -> (String, Option<String>) {
    match status {
        Some(raw) => {
            let normalized = raw.to_ascii_lowercase();
            if normalized.contains("fail") || normalized.contains("error") {
                ("failed".to_string(), Some(raw))
            } else if normalized.contains("success") || normalized.contains("complete") {
                ("completed".to_string(), None)
            } else if normalized.contains("pending")
                || normalized.contains("sent")
                || normalized.contains("submit")
                || normalized.contains("queue")
            {
                ("sent".to_string(), None)
            } else {
                ("sent".to_string(), None)
            }
        }
        None => ("initiated".to_string(), None),
    }
}

fn reveal_or_encrypted(value: Option<String>) -> String {
    match value {
        Some(v) if !v.trim().is_empty() => v,
        _ => "encrypted".to_string(),
    }
}

fn record_from_indexer(
    tx_hash: String,
    status: Option<String>,
    timestamp_ms: i64,
    sender: Option<String>,
    recipient: Option<String>,
    privacy_sender: Option<String>,
    privacy_recipient: Option<String>,
    amount: Option<String>,
) -> TransactionRecord {
    let (state, error_message) = map_state_and_error(status.clone());
    let from_address = privacy_sender.or(sender);
    let to_address = privacy_recipient.or(recipient);

    TransactionRecord {
        id: tx_hash.clone(),
        state,
        from_address: reveal_or_encrypted(from_address),
        to_address: reveal_or_encrypted(to_address),
        amount: reveal_or_encrypted(amount),
        tx_identifier: Some(tx_hash),
        created_at: timestamp_ms,
        updated_at: timestamp_ms,
        error_message,
    }
}

fn transaction_to_record(tx: crate::operations::Transaction) -> TransactionRecord {
    record_from_indexer(
        tx.tx_hash,
        tx.status,
        tx.timestamp_ms,
        tx.sender,
        tx.recipient,
        tx.privacy_sender,
        tx.privacy_recipient,
        tx.amount,
    )
}

fn details_to_record(details: crate::operations::TransactionDetails) -> TransactionRecord {
    record_from_indexer(
        details.tx_hash,
        Some(details.status),
        details.timestamp_ms.unwrap_or_default(),
        details.sender,
        details.recipient,
        details.privacy_sender,
        details.privacy_recipient,
        details.amount,
    )
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
