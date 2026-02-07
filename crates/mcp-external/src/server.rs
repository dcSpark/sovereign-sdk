#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use crate::commitment_tree::global_tree_syncer;
use crate::fvk_service::{fetch_viewer_fvk_bundle, parse_hex_32, ViewerFvkBundle};
use crate::ligero::Ligero as LigeroProver;
use crate::prefunded_wallets::PrefundedWalletStore;
use crate::privacy_key::PrivacyKey;
use crate::provider::Provider;
use crate::session_store::{PendingSpentNoteSnapshot, SessionSnapshot, SessionStore};
use crate::wallet::WalletContext;
use demo_stf::runtime::Runtime;
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey};
use midnight_privacy::{note_commitment, Hash32};
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
use sov_api_spec::types::TxReceiptResult;
use sov_bank::config_gas_token_id;
use sov_ligero_adapter::Ligero;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::{Amount, Spec};
use tokio::sync::{Mutex, RwLock};

pub type McpSpec = ConfigurableSpec<MockDaSpec, Ligero, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;
pub type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];

pub(crate) async fn run_auto_fund_sequence(
    provider: Arc<Provider>,
    admin_ctx: Arc<McpWalletContext>,
    dest_wallet_address: String,
    dest_privacy_key: PrivacyKey,
    new_wallet_for_deposit: Arc<McpWalletContext>,
    deposit_amount: u128,
    auto_fund_gas_reserve: u128,
) -> anyhow::Result<()> {
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
            let tx_hash = res.tx_hash.trim().to_string();
            tracing::info!(
                "[auto-fund/createWallet] Step 1 complete: L2 funding tx {}",
                tx_hash
            );
            if tx_hash.is_empty() {
                tracing::warn!(
                    "[auto-fund/createWallet] L2 funding tx hash is empty; skipping Step 2."
                );
                anyhow::bail!("L2 funding tx hash is empty");
            }

            let tx_max_wait = std::time::Duration::from_secs(300);
            let tx_poll_interval = std::time::Duration::from_secs(2);
            let tx_started = std::time::Instant::now();

            loop {
                match provider.get_sequencer_tx(&tx_hash).await {
                    Ok(Some(tx)) => match &tx.receipt.result {
                        TxReceiptResult::Successful => {
                            tracing::info!(
                                "[auto-fund/createWallet] L2 funding tx accepted by sequencer: receipt={:?}",
                                tx.receipt.result
                            );
                            break;
                        }
                        TxReceiptResult::Reverted | TxReceiptResult::Skipped => {
                            tracing::warn!(
                                    "[auto-fund/createWallet] L2 funding tx failed in sequencer: receipt={:?}. Skipping Step 2.",
                                    tx.receipt.result
                                );
                            anyhow::bail!(
                                "L2 funding tx failed in sequencer: receipt={:?}",
                                tx.receipt.result
                            );
                        }
                    },
                    Ok(None) => {
                        tracing::info!(
                            "[auto-fund/createWallet] Waiting for L2 funding tx {} to appear in sequencer",
                            tx_hash
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[auto-fund/createWallet] Failed to query sequencer for L2 funding tx receipt: {}",
                            e
                        );
                    }
                }

                if tx_started.elapsed() >= tx_max_wait {
                    tracing::warn!(
                        "[auto-fund/createWallet] Timed out waiting for L2 funding tx in sequencer; skipping Step 2."
                    );
                    anyhow::bail!("Timed out waiting for L2 funding tx in sequencer");
                }

                tokio::time::sleep(tx_poll_interval).await;
            }

            let dest_wallet_address_parsed: <McpSpec as Spec>::Address = match dest_wallet_address
                .parse()
            {
                Ok(address) => address,
                Err(e) => {
                    tracing::warn!(
                            "[auto-fund/createWallet] Invalid L2 wallet address '{}': {}. Skipping Step 2.",
                            dest_wallet_address,
                            e
                        );
                    anyhow::bail!("Invalid L2 wallet address '{}': {}", dest_wallet_address, e);
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
                    anyhow::bail!("Timed out waiting for L2 funding balance");
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
                Ok(res) => {
                    tracing::info!(
                        "[auto-fund/createWallet] Step 2 complete: Privacy pool deposit tx {}",
                        res.tx_hash
                    );
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        "[auto-fund/createWallet] Step 2 failed (privacy pool deposit): {}",
                        e
                    );
                    Err(anyhow::anyhow!(
                        "Step 2 failed (privacy pool deposit): {}",
                        e
                    ))
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "[auto-fund/createWallet] Step 1 failed (L2 funding): {}. Skipping Step 2.",
                e
            );
            Err(anyhow::anyhow!("Step 1 failed (L2 funding): {}", e))
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
    /// Post-submit confirmation status.
    pub confirmation: String,
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
    /// Transaction events from the rollup (NoteCreated, NoteSpent, NoteEncrypted, PoolTransfer, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<serde_json::Value>,
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

const DEFAULT_PENDING_SPENT_NOTE_TTL_SECS: u64 = 120;
const DEFAULT_WAIT_FOR_FRESH_NOTES_SECS: u64 = 5;
const DEFAULT_WAIT_FOR_TREE_VISIBLE_NOTES_SECS: u64 = 60;
const NOTES_WAIT_POLL_MS: u64 = 500;
const NOTES_WAIT_PROGRESS_LOG_SECS: u64 = 5;
const DEFAULT_TREE_RESOLVE_RETRY_ATTEMPTS: u32 = 1;
const DEFAULT_TREE_RESOLVE_RETRY_DELAY_MS: u64 = 750;
const DEFAULT_LOCAL_NOTES_TREE_BYPASS: bool = false;
const DEFAULT_TREE_PRESENCE_SYNC_EVERY_POLLS: u64 = 4;

fn pending_spent_note_ttl() -> std::time::Duration {
    let secs = std::env::var("MCP_PENDING_SPENT_NOTE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PENDING_SPENT_NOTE_TTL_SECS);
    std::time::Duration::from_secs(secs)
}

fn wait_for_fresh_notes_secs() -> u64 {
    std::env::var("MCP_WAIT_FOR_FRESH_NOTES_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WAIT_FOR_FRESH_NOTES_SECS)
}

fn wait_for_tree_visible_notes_secs() -> u64 {
    std::env::var("MCP_WAIT_FOR_TREE_VISIBLE_NOTES_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WAIT_FOR_TREE_VISIBLE_NOTES_SECS)
}

fn tree_resolve_retry_attempts() -> u32 {
    std::env::var("MCP_TREE_RESOLVE_RETRY_ATTEMPTS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_TREE_RESOLVE_RETRY_ATTEMPTS)
}

fn tree_resolve_retry_delay_ms() -> u64 {
    std::env::var("MCP_TREE_RESOLVE_RETRY_DELAY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TREE_RESOLVE_RETRY_DELAY_MS)
}

fn tree_presence_sync_every_polls() -> u64 {
    std::env::var("MCP_TREE_PRESENCE_SYNC_EVERY_POLLS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TREE_PRESENCE_SYNC_EVERY_POLLS)
        .max(1)
}

fn local_notes_tree_bypass_enabled() -> bool {
    std::env::var("MCP_LOCAL_NOTES_TREE_BYPASS")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(DEFAULT_LOCAL_NOTES_TREE_BYPASS)
}

fn is_tree_positions_resolution_error(error_text: &str) -> bool {
    error_text.contains("Failed to resolve Merkle positions/openings from cached commitment tree")
}

fn is_invalid_anchor_root_error(error_text: &str) -> bool {
    error_text.contains("Invalid anchor root")
}

fn note_commitment_for_owner(
    note: &crate::operations::SpendableNote,
    owner_recipient: &Hash32,
) -> Option<Hash32> {
    let value = u64::try_from(note.value).ok()?;
    let rho = parse_hex_32("rho", &note.rho).ok()?;
    let sender_id = parse_hex_32("sender_id", &note.sender_id).ok()?;
    Some(note_commitment(
        &DOMAIN,
        value,
        &rho,
        owner_recipient,
        &sender_id,
    ))
}

#[derive(Debug, Default)]
pub(crate) struct PendingSpentNotes {
    pub(crate) by_rho: HashMap<String, std::time::SystemTime>,
}

impl PendingSpentNotes {
    fn purge_expired(&mut self) {
        let ttl = pending_spent_note_ttl();
        let now = std::time::SystemTime::now();
        self.by_rho
            .retain(|_, inserted_at| match now.duration_since(*inserted_at) {
                Ok(elapsed) => elapsed < ttl,
                // Keep entries when local clock moves backwards.
                Err(_) => true,
            });
    }

    fn snapshot_entries(&mut self) -> Vec<PendingSpentNoteSnapshot> {
        self.purge_expired();
        self.by_rho
            .iter()
            .map(|(rho, inserted_at)| {
                let inserted_at_ms = inserted_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                PendingSpentNoteSnapshot {
                    rho: rho.clone(),
                    inserted_at_ms,
                }
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub(crate) struct LocalNotes {
    pub(crate) by_rho: HashMap<String, crate::operations::SpendableNote>,
}

#[derive(Clone)]
pub struct CryptoServer {
    tool_router: ToolRouter<Self>,
    provider: Option<Arc<Provider>>,
    wallet_context: Arc<RwLock<Option<McpWalletContext>>>,
    admin_wallet_context: Option<Arc<McpWalletContext>>,
    ligero_prover: Option<Arc<LigeroProver>>,
    viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
    privacy_key: Arc<RwLock<Option<PrivacyKey>>>,
    prefunded_wallets: Option<Arc<PrefundedWalletStore>>,
    log_path: String,
    auto_fund_deposit_amount: Option<u128>,
    auto_fund_gas_reserve: u128,
    /// Tracks whether a wallet has been explicitly loaded via createWallet or restoreWallet.
    /// When true, createWallet and restoreWallet will fail until removeWallet is called.
    wallet_explicitly_loaded: Arc<RwLock<bool>>,
    session_id: Option<String>,
    session_store: Option<Arc<SessionStore>>,
    /// Best-effort local cache of recently-spent note identifiers (rho hex), used to avoid
    /// double-spending when the indexer lags behind the sequencer.
    pending_spent_notes: Arc<Mutex<PendingSpentNotes>>,
    /// Best-effort local cache of newly-created owned notes, so consecutive sends don't have to
    /// wait for indexer lag.
    local_notes: Arc<Mutex<LocalNotes>>,
}

#[allow(rust_analyzer::macro_error)]
#[tool_router]
impl CryptoServer {
    pub(crate) fn new(
        provider: Arc<Provider>,
        wallet_context: Arc<RwLock<Option<McpWalletContext>>>,
        admin_wallet_context: Option<Arc<McpWalletContext>>,
        ligero_prover: Arc<LigeroProver>,
        viewer_fvk_bundle: Arc<RwLock<Option<ViewerFvkBundle>>>,
        privacy_key: Arc<RwLock<Option<PrivacyKey>>>,
        prefunded_wallets: Option<Arc<PrefundedWalletStore>>,
        log_path: String,
        auto_fund_deposit_amount: Option<u128>,
        auto_fund_gas_reserve: u128,
        wallet_explicitly_loaded: Arc<RwLock<bool>>,
        session_id: Option<String>,
        session_store: Option<Arc<SessionStore>>,
        pending_spent_notes: Arc<Mutex<PendingSpentNotes>>,
        local_notes: Arc<Mutex<LocalNotes>>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            provider: Some(provider),
            wallet_context,
            admin_wallet_context,
            ligero_prover: Some(ligero_prover),
            viewer_fvk_bundle,
            privacy_key,
            prefunded_wallets,
            log_path,
            auto_fund_deposit_amount,
            auto_fund_gas_reserve,
            wallet_explicitly_loaded,
            session_id,
            session_store,
            pending_spent_notes,
            local_notes,
        }
    }

    async fn persist_session_snapshot(&self, snapshot: SessionSnapshot) {
        let Some(store) = self.session_store.as_ref() else {
            return;
        };
        let Some(session_id) = self.session_id.as_deref() else {
            tracing::warn!("[mcp] Session persistence enabled but session id is missing");
            return;
        };
        if let Err(err) = store.save_session(session_id, &snapshot).await {
            tracing::warn!("[mcp] Failed to persist session {session_id}: {err}");
        }
    }

    async fn clear_session_snapshot(&self) {
        let Some(store) = self.session_store.as_ref() else {
            return;
        };
        let Some(session_id) = self.session_id.as_deref() else {
            tracing::warn!("[mcp] Session persistence enabled but session id is missing");
            return;
        };
        if let Err(err) = store.delete_session(session_id).await {
            tracing::warn!("[mcp] Failed to delete session {session_id}: {err}");
        }
    }

    async fn persist_note_caches_to_session_snapshot(&self) {
        let Some(store) = self.session_store.as_ref() else {
            return;
        };
        let Some(session_id) = self.session_id.as_deref() else {
            tracing::warn!("[mcp] Session persistence enabled but session id is missing");
            return;
        };

        let mut snapshot = match store.load_session(session_id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("[mcp] Failed to load session {session_id} for cache update: {err}");
                return;
            }
        };

        let pending_spent_notes = {
            let mut pending = self.pending_spent_notes.lock().await;
            pending.snapshot_entries()
        };
        let local_notes = {
            let local = self.local_notes.lock().await;
            local.by_rho.values().cloned().collect()
        };

        snapshot.pending_spent_notes = pending_spent_notes;
        snapshot.local_notes = local_notes;

        if let Err(err) = store.save_session(session_id, &snapshot).await {
            tracing::warn!("[mcp] Failed to persist note caches for session {session_id}: {err}");
        }
    }

    /// Send funds from the privacy pool using up to 4 unspent notes (largest-first).
    #[tool(
        name = "send",
        description = "Send funds from the privacy pool to a destination privacy address. Selects up to 4 unspent notes (largest-first) and submits a privacy transfer."
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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
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
        let privacy_key = privacy_key_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;
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

        let from_privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();
        let send_span = tracing::info_span!(
            "send",
            from = %from_privacy_address,
            to = %output_privacy_addr,
            amount = send_amount
        );
        let _send_guard = send_span.enter();
        let send_started = std::time::Instant::now();

        let notes_wait_started = std::time::Instant::now();
        let fresh_notes_wait_limit = std::time::Duration::from_secs(wait_for_fresh_notes_secs());
        let tree_visible_wait_limit = std::time::Duration::from_secs(
            wait_for_fresh_notes_secs().max(wait_for_tree_visible_notes_secs()),
        );
        let mut notes_wait_last_progress_log = notes_wait_started;
        let mut notes_fetch_attempts: u64 = 0;
        let tree_presence_sync_every_polls_cfg = tree_presence_sync_every_polls();
        let mut tree_sync_round = global_tree_syncer().current_sync_round();
        let mut notes_fetch_ms_total: u128 = 0;
        let mut notes_filtered_pending_total: u64 = 0;
        let mut notes_added_local_total: u64 = 0;
        let mut notes_filtered_tree_missing_total: u64 = 0;
        let mut notes_unparsable_total: u64 = 0;
        let mut notes_tree_blocked_covering = false;
        let mut notes_returned_by_indexer_last: usize;
        let owner_recipient = privacy_key.recipient(&DOMAIN);

        let notes = loop {
            notes_fetch_attempts += 1;
            let fetch_started = std::time::Instant::now();
            let mut notes =
                crate::operations::get_privacy_notes(provider, privacy_key, Some(&viewing_key))
                    .await
                    .map_err(|e| {
                        ErrorData::internal_error(
                            format!("Failed to fetch unspent notes: {}", e),
                            None,
                        )
                    })?;
            notes_fetch_ms_total += fetch_started.elapsed().as_millis();
            notes_returned_by_indexer_last = notes.len();

            let local_notes: Vec<crate::operations::SpendableNote> = {
                let local = self.local_notes.lock().await;
                local.by_rho.values().cloned().collect()
            };
            let filtered = {
                let mut pending = self.pending_spent_notes.lock().await;
                pending.purge_expired();
                let before = notes.len();
                notes.retain(|n| !pending.by_rho.contains_key(&n.rho));
                before.saturating_sub(notes.len())
            };
            if filtered > 0 {
                notes_filtered_pending_total += filtered as u64;
                tracing::debug!(
                    "[send] Filtered {} locally-pending spent note(s) from indexer results",
                    filtered
                );
            }

            // Compute local_note_rhos BEFORE consuming local_notes via into_iter.
            let local_note_rhos: std::collections::HashSet<String> =
                local_notes.iter().map(|note| note.rho.clone()).collect();

            if !local_notes.is_empty() {
                let mut seen: std::collections::HashSet<String> =
                    notes.iter().map(|n| n.rho.clone()).collect();
                let mut added = 0usize;
                for note in local_notes {
                    if seen.insert(note.rho.clone()) {
                        notes.push(note);
                        added += 1;
                    }
                }
                if added > 0 {
                    notes_added_local_total += added as u64;
                    tracing::debug!(
                        "[send] Added {} locally-cached owned note(s) to candidates",
                        added
                    );
                }
            }

            // Only use notes whose commitments are already visible in the current commitment tree.
            // This avoids selecting very fresh notes that indexer can see before tree endpoints catch up.
            let mut notes_with_cm: Vec<(crate::operations::SpendableNote, Hash32)> =
                Vec::with_capacity(notes.len());
            for note in notes {
                if let Some(cm) = note_commitment_for_owner(&note, &owner_recipient) {
                    notes_with_cm.push((note, cm));
                } else {
                    notes_unparsable_total += 1;
                }
            }
            let cms: Vec<Hash32> = notes_with_cm.iter().map(|(_, cm)| *cm).collect();
            let mut presence = global_tree_syncer().commitment_presence_cached(&cms).await;
            let cache_complete = presence.iter().all(|present| *present);
            if !cache_complete
                && (notes_fetch_attempts == 1
                    || notes_fetch_attempts % tree_presence_sync_every_polls_cfg == 0)
            {
                presence = global_tree_syncer()
                    .commitment_presence(provider, &cms)
                    .await
                    .map_err(|e| {
                        ErrorData::internal_error(
                            format!(
                                "Failed to verify note commitments in commitment tree: {}",
                                e
                            ),
                            None,
                        )
                    })?;
                tree_sync_round = global_tree_syncer().current_sync_round();
            }
            if tracing::enabled!(tracing::Level::DEBUG) {
                let owner_recipient_hex = hex::encode(owner_recipient);
                for ((note, cm), present) in notes_with_cm.iter().zip(presence.iter()).take(6) {
                    tracing::debug!(
                        owner_recipient = %owner_recipient_hex,
                        note_value = note.value,
                        note_rho = %note.rho,
                        note_sender_id = %note.sender_id,
                        note_kind = %note.kind,
                        note_tx_hash = %note.tx_hash,
                        computed_cm = %hex::encode(cm),
                        present_in_tree = *present,
                        "[send] Candidate note commitment presence"
                    );
                }
            }
            let before_tree_filter = notes_with_cm.len();
            let candidate_notes: Vec<crate::operations::SpendableNote> =
                notes_with_cm.iter().map(|(note, _)| note.clone()).collect();
            let mut tree_visible_notes = Vec::with_capacity(before_tree_filter);
            let local_tree_bypass = local_notes_tree_bypass_enabled();
            for ((note, _), present) in notes_with_cm.into_iter().zip(presence.into_iter()) {
                // When enabled via MCP_LOCAL_NOTES_TREE_BYPASS=1, local notes can bypass
                // tree visibility at selection time. This is disabled by default because
                // under sequencer congestion local outputs may not become tree-visible
                // quickly enough and cause repeated pre-transfer timeouts.
                if present || (local_tree_bypass && local_note_rhos.contains(&note.rho)) {
                    tree_visible_notes.push(note);
                }
            }
            let filtered_tree = before_tree_filter.saturating_sub(tree_visible_notes.len());
            if filtered_tree > 0 {
                notes_filtered_tree_missing_total += filtered_tree as u64;
                tracing::debug!(
                    "[send] Filtered {} note(s) not yet visible in commitment tree",
                    filtered_tree
                );
            }

            let has_covering_candidates = crate::operations::select_largest_notes_covering_amount(
                candidate_notes,
                send_amount,
                crate::viewer::MAX_INS,
            )
            .is_ok();
            let has_covering_notes = crate::operations::select_largest_notes_covering_amount(
                tree_visible_notes.clone(),
                send_amount,
                crate::viewer::MAX_INS,
            )
            .is_ok();

            if has_covering_candidates && !has_covering_notes {
                notes_tree_blocked_covering = true;
            }

            // When we have local in-flight state (recent pending spends or locally-cached outputs),
            // short fresh-note waits are too aggressive under sequencer/indexer congestion.
            // Use the longer tree-visible window so retries don't fail prematurely.
            let has_inflight_note_state = filtered > 0 || !local_note_rhos.is_empty();
            let notes_wait_limit = if notes_tree_blocked_covering || has_inflight_note_state {
                tree_visible_wait_limit
            } else {
                fresh_notes_wait_limit
            };

            if has_covering_notes || notes_wait_started.elapsed() >= notes_wait_limit {
                break tree_visible_notes;
            }

            if notes_wait_last_progress_log.elapsed().as_secs() >= NOTES_WAIT_PROGRESS_LOG_SECS {
                tracing::info!(
                    elapsed_ms = notes_wait_started.elapsed().as_millis(),
                    wait_limit_ms = notes_wait_limit.as_millis(),
                    notes_fetch_attempts,
                    notes_candidates_with_cm = before_tree_filter,
                    notes_tree_visible = tree_visible_notes.len(),
                    notes_tree_blocked_covering,
                    "Waiting for spendable notes"
                );
                notes_wait_last_progress_log = std::time::Instant::now();
            }

            let poll_delay = std::time::Duration::from_millis(NOTES_WAIT_POLL_MS);
            if global_tree_syncer()
                .wait_for_sync_round_advance(tree_sync_round, poll_delay)
                .await
            {
                tree_sync_round = global_tree_syncer().current_sync_round();
            }
        };

        let notes_wait_ms = notes_wait_started.elapsed().as_millis();
        tracing::debug!(
            notes_wait_ms,
            notes_fetch_attempts,
            notes_fetch_ms_total,
            notes_returned_by_indexer_last,
            notes_filtered_pending_total,
            notes_added_local_total,
            notes_filtered_tree_missing_total,
            notes_unparsable_total,
            notes_candidates = notes.len(),
            "Unspent notes ready"
        );

        if notes.is_empty() {
            if notes_tree_blocked_covering || notes_filtered_tree_missing_total > 0 {
                return Err(ErrorData::invalid_params(
                    format!(
                        "Spendable notes were found but are not yet visible in the commitment tree after waiting {}s. Retry shortly.",
                        tree_visible_wait_limit.as_secs()
                    ),
                    None,
                ));
            }
            return Err(ErrorData::invalid_params(
                "No unspent notes available to send.".to_string(),
                None,
            ));
        }

        let selection_started = std::time::Instant::now();
        let selected = match crate::operations::select_largest_notes_covering_amount(
            notes,
            send_amount,
            crate::viewer::MAX_INS,
        ) {
            Ok(selected) => selected,
            Err(e) => {
                if notes_filtered_tree_missing_total > 0 || notes_tree_blocked_covering {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "Inputs are not yet visible in the commitment tree (filtered {} candidate notes). Retry shortly.",
                            notes_filtered_tree_missing_total
                        ),
                        None,
                    ));
                }
                return Err(ErrorData::invalid_params(
                    format!("Insufficient funds: {}", e),
                    None,
                ));
            }
        };

        let total_in: u128 = selected.iter().map(|n| n.value).sum();
        let selection_ms = selection_started.elapsed().as_millis();
        tracing::info!(
            elapsed_ms = selection_ms,
            selected_inputs = selected.len(),
            total_in,
            "Selected input notes"
        );

        tracing::debug!(
            "[send] Selected {} input notes (total_in={}, send_amount={})",
            selected.len(),
            total_in,
            send_amount
        );

        let inputs_started = std::time::Instant::now();
        let mut inputs: Vec<crate::operations::TransferInputNote> =
            Vec::with_capacity(selected.len());
        for (idx, n) in selected.iter().enumerate() {
            let rho = parse_hex_32("rho", &n.rho).map_err(|e| {
                ErrorData::internal_error(
                    format!("Failed to decode rho for input {}: {}", idx, e),
                    None,
                )
            })?;
            let sender_id = parse_hex_32("sender_id", &n.sender_id).map_err(|e| {
                ErrorData::internal_error(
                    format!("Failed to decode sender_id for input {}: {}", idx, e),
                    None,
                )
            })?;

            tracing::debug!(
                "[send] Input[{}] value={} rho={} sender_id={} created_tx={}",
                idx,
                n.value,
                &n.rho,
                &n.sender_id,
                n.tx_hash
            );

            inputs.push(crate::operations::TransferInputNote {
                value: n.value,
                rho,
                sender_id,
            });
        }
        let inputs_ms = inputs_started.elapsed().as_millis();
        tracing::debug!(elapsed_ms = inputs_ms, "Prepared transfer inputs");
        let output_recipient = recipient_from_pk_v2(&DOMAIN, &output_pk, &output_pk_ivk);

        tracing::debug!(
            "[send] Output recipient (destination): {}",
            hex::encode(&output_recipient)
        );

        if total_in > send_amount {
            let change_amt = total_in - send_amount;
            tracing::debug!(
                "[send] Transfer includes change output - amount: {}",
                change_amt
            );
        } else {
            tracing::debug!("[send] No change needed - sending full note value");
        }

        let spend_sk = privacy_key.spend_sk().copied().ok_or_else(|| {
            ErrorData::internal_error(
                "privacy key missing spend_sk; cannot spend note".to_string(),
                None,
            )
        })?;
        let pk_ivk_owner = privacy_key.pk_ivk(&DOMAIN);
        let sender_id_out =
            midnight_privacy::recipient_from_sk_v2(&DOMAIN, &spend_sk, &pk_ivk_owner);
        let ligero_ref = self.ligero_prover.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "Ligero proof service not configured; set LIGERO_PROOF_SERVICE_URL.".to_string(),
                None,
            )
        })?;
        // --- Pre-transfer tree-presence wait ---
        // The notes-wait loop above may have included local notes that bypass the tree-
        // visibility check.  Before attempting the expensive proof generation (which
        // needs Merkle openings from the tree), ensure all selected input commitments
        // are actually present in the cached commitment tree.
        {
            let selected_cms: Vec<midnight_privacy::Hash32> = selected
                .iter()
                .filter_map(|n| note_commitment_for_owner(n, &owner_recipient))
                .collect();
            if !selected_cms.is_empty() {
                let tree_wait_limit =
                    std::time::Duration::from_secs(wait_for_tree_visible_notes_secs());
                let tree_sync_every_polls = tree_presence_sync_every_polls();
                let mut tree_sync_round = global_tree_syncer().current_sync_round();
                let tree_wait_started = std::time::Instant::now();
                let mut tree_wait_polls: u64 = 0;
                let mut tree_wait_last_progress_log = std::time::Instant::now();
                let mut last_tree_sync_error: Option<String> = None;
                loop {
                    tree_wait_polls += 1;
                    let presence = global_tree_syncer()
                        .commitment_presence_cached(&selected_cms)
                        .await;
                    let mut all_present = presence.iter().all(|p| *p);
                    if all_present {
                        last_tree_sync_error = None;
                    } else if tree_wait_polls == 1 || tree_wait_polls % tree_sync_every_polls == 0 {
                        match global_tree_syncer()
                            .commitment_presence(provider, &selected_cms)
                            .await
                        {
                            Ok(v) => {
                                last_tree_sync_error = None;
                                all_present = v.iter().all(|p| *p);
                                tree_sync_round = global_tree_syncer().current_sync_round();
                            }
                            Err(e) => {
                                let error_text = format!("{:#}", e);
                                last_tree_sync_error = Some(error_text.clone());
                                if tree_wait_last_progress_log.elapsed().as_secs()
                                    >= NOTES_WAIT_PROGRESS_LOG_SECS
                                {
                                    tracing::warn!(
                                        elapsed_ms = tree_wait_started.elapsed().as_millis(),
                                        polls = tree_wait_polls,
                                        inputs = selected_cms.len(),
                                        error = %error_text,
                                        "Pre-transfer commitment-tree sync failed while waiting for selected inputs"
                                    );
                                    tree_wait_last_progress_log = std::time::Instant::now();
                                }
                            }
                        }
                    }
                    if all_present {
                        let elapsed = tree_wait_started.elapsed();
                        if elapsed.as_millis() > 100 {
                            tracing::info!(
                                elapsed_ms = elapsed.as_millis(),
                                polls = tree_wait_polls,
                                inputs = selected_cms.len(),
                                "Pre-transfer tree-presence wait completed"
                            );
                        }
                        break;
                    }
                    if tree_wait_started.elapsed() >= tree_wait_limit {
                        // If we timed out waiting for selected inputs to become tree-visible,
                        // evict any selected entries from the local note cache so they are not
                        // repeatedly re-selected forever in subsequent retries.
                        let mut evicted_local_notes = 0usize;
                        {
                            let mut local = self.local_notes.lock().await;
                            for note in &selected {
                                if local.by_rho.remove(&note.rho).is_some() {
                                    evicted_local_notes += 1;
                                }
                            }
                        }
                        if evicted_local_notes > 0 {
                            tracing::warn!(
                                evicted_local_notes,
                                "Evicted stale local notes after tree-presence timeout"
                            );
                            self.persist_note_caches_to_session_snapshot().await;
                        }
                        tracing::warn!(
                            elapsed_ms = tree_wait_started.elapsed().as_millis(),
                            polls = tree_wait_polls,
                            inputs = selected_cms.len(),
                            "Selected inputs not visible in commitment tree after waiting"
                        );
                        let detail = last_tree_sync_error
                            .as_deref()
                            .map(|e| e.chars().take(280).collect::<String>());
                        return Err(ErrorData::invalid_params(
                            match detail {
                                Some(detail) => format!(
                                    "Transfer inputs are not yet visible in the commitment tree after waiting {}s. Last tree sync error: {}",
                                    tree_wait_limit.as_secs(),
                                    detail
                                ),
                                None => format!(
                                    "Transfer inputs are not yet visible in the commitment tree after waiting {}s. Retry shortly.",
                                    tree_wait_limit.as_secs()
                                ),
                            },
                            None,
                        ));
                    }
                    let poll_delay = std::time::Duration::from_millis(NOTES_WAIT_POLL_MS);
                    if global_tree_syncer()
                        .wait_for_sync_round_advance(tree_sync_round, poll_delay)
                        .await
                    {
                        tree_sync_round = global_tree_syncer().current_sync_round();
                    }
                }
            }
        }

        let transfer_started = std::time::Instant::now();
        let tree_retry_attempts = tree_resolve_retry_attempts();
        let tree_retry_delay = std::time::Duration::from_millis(tree_resolve_retry_delay_ms());
        let mut transfer_attempt: u32 = 0;
        let transfer_result = loop {
            transfer_attempt += 1;
            let res = crate::operations::transfer(
                ligero_ref,
                provider,
                ctx,
                spend_sk,
                pk_ivk_owner,
                send_amount,
                inputs.clone(),
                output_pk,
                output_pk_ivk,
                viewer_fvk_bundle_for_transfer.clone(),
            )
            .await;

            match res {
                Ok(ok) => break Ok(ok),
                Err(e) => {
                    // Use {:#} to serialize the full anyhow error chain, not just
                    // the outermost .context(). Without this, nested causes like
                    // "Nullifier already spent" are invisible to pattern matching.
                    let error_text = format!("{:#}", e);
                    if is_tree_positions_resolution_error(&error_text)
                        && transfer_attempt <= tree_retry_attempts + 1
                    {
                        tracing::warn!(
                            attempt = transfer_attempt,
                            max_attempts = tree_retry_attempts + 1,
                            retry_delay_ms = tree_retry_delay.as_millis(),
                            error = %error_text,
                            "Transfer hit transient commitment-tree lag; retrying"
                        );
                        tokio::time::sleep(tree_retry_delay).await;
                        continue;
                    }
                    if is_invalid_anchor_root_error(&error_text)
                        && transfer_attempt <= tree_retry_attempts + 1
                    {
                        tracing::warn!(
                            attempt = transfer_attempt,
                            max_attempts = tree_retry_attempts + 1,
                            retry_delay_ms = tree_retry_delay.as_millis(),
                            error = %error_text,
                            "Transfer rejected due to stale/invalid anchor root; resetting commitment-tree cache and retrying"
                        );
                        global_tree_syncer().reset_cache().await;
                        tokio::time::sleep(tree_retry_delay).await;
                        continue;
                    }
                    break Err(e);
                }
            }
        };
        let transfer_result = match transfer_result {
            Ok(ok) => ok,
            Err(e) => {
                // Use {:#} to serialize the full anyhow error chain so that
                // nested causes (e.g. "Nullifier already spent" inside
                // "Failed to submit transaction to verifier service") are
                // visible to the pattern-matching checks below.
                let error_text = format!("{:#}", e);
                let nullifier_spent = error_text.contains("Nullifier already spent");
                let verifier_client_reject = error_text.contains("error status 4");
                let tree_resolution_error = is_tree_positions_resolution_error(&error_text);

                if nullifier_spent {
                    tracing::warn!(
                        selected_inputs = selected.len(),
                        error = %error_text,
                        "Transfer rejected due to already-spent nullifier; marking selected notes as locally pending-spent to avoid immediate re-selection"
                    );

                    // Mark selected inputs as locally pending-spent so immediate retries
                    // don't keep selecting the same stale notes while indexer state catches up.
                    {
                        let mut pending = self.pending_spent_notes.lock().await;
                        let mut local = self.local_notes.lock().await;
                        pending.purge_expired();
                        for note in &selected {
                            pending
                                .by_rho
                                .insert(note.rho.clone(), std::time::SystemTime::now());
                            local.by_rho.remove(&note.rho);
                        }
                    }

                    self.persist_note_caches_to_session_snapshot().await;

                    return Err(ErrorData::invalid_params(
                        "Transfer rejected: selected note is already spent (nullifier already spent). This is usually temporary indexer lag; retry shortly."
                            .to_string(),
                        None,
                    ));
                }

                if verifier_client_reject {
                    return Err(ErrorData::invalid_params(
                        format!("Transfer rejected by verifier/sequencer: {error_text}"),
                        None,
                    ));
                }

                if tree_resolution_error {
                    return Err(ErrorData::invalid_params(
                        "Transfer inputs are not yet visible in the commitment tree (transient lag under load). Retry shortly."
                            .to_string(),
                        None,
                    ));
                }

                return Err(ErrorData::internal_error(
                    format!("Failed to submit privacy transfer: {error_text}"),
                    None,
                ));
            }
        };
        let transfer_ms = transfer_started.elapsed().as_millis();
        tracing::debug!(
            elapsed_ms = transfer_ms,
            tx_hash = %transfer_result.tx_hash,
            "Transfer call completed"
        );

        {
            let mut pending = self.pending_spent_notes.lock().await;
            let mut local = self.local_notes.lock().await;
            pending.purge_expired();
            for note in &selected {
                pending
                    .by_rho
                    .insert(note.rho.clone(), std::time::SystemTime::now());
                local.by_rho.remove(&note.rho);
            }

            // Cache owned primary outputs immediately so follow-up sends do not depend on indexer
            // timing. This is critical for send-to-self flows where there may be no change.
            if transfer_result.output_recipient == owner_recipient {
                let rho_hex = hex::encode(transfer_result.output_rho);
                let sender_id_hex = hex::encode(sender_id_out);
                local.by_rho.insert(
                    rho_hex.clone(),
                    crate::operations::SpendableNote {
                        value: send_amount,
                        rho: rho_hex,
                        sender_id: sender_id_hex,
                        tx_hash: transfer_result.tx_hash.clone(),
                        timestamp_ms: transfer_result.created_at,
                        kind: "transfer".to_string(),
                    },
                );
            }

            if let (Some(change_amount), Some(change_rho)) =
                (transfer_result.change_amount, transfer_result.change_rho)
            {
                let rho_hex = hex::encode(change_rho);
                let sender_id_hex = hex::encode(sender_id_out);
                local.by_rho.insert(
                    rho_hex.clone(),
                    crate::operations::SpendableNote {
                        value: change_amount,
                        rho: rho_hex,
                        sender_id: sender_id_hex,
                        tx_hash: transfer_result.tx_hash.clone(),
                        timestamp_ms: transfer_result.created_at,
                        kind: "transfer".to_string(),
                    },
                );
            }
        }
        self.persist_note_caches_to_session_snapshot().await;

        let created_at = transfer_result.created_at;
        let total_ms = send_started.elapsed().as_millis();
        tracing::info!(
            total_ms,
            notes_wait_ms,
            selection_ms,
            inputs_ms,
            transfer_ms,
            tx_hash = %transfer_result.tx_hash,
            "Send completed"
        );

        let result = SendFundsResult {
            id: transfer_result.tx_hash,
            state: "initiated".to_string(),
            confirmation: transfer_result.confirmation.as_str().to_string(),
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

        let viewer_fvk_guard = self.viewer_fvk_bundle.read().await;
        let viewing_key = viewer_fvk_guard
            .as_ref()
            .map(|bundle| midnight_privacy::FullViewingKey(bundle.fvk));

        let privacy_key_guard = self.privacy_key.read().await;
        let privacy_key = privacy_key_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let privacy_result =
            crate::operations::get_privacy_balance(provider, privacy_key, viewing_key.as_ref())
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
        let privacy_key = privacy_key_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;
        let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;
        let privacy_key_guard = self.privacy_key.read().await;
        let privacy_key = privacy_key_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let transactions = crate::operations::get_transactions(provider, ctx, privacy_key)
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

        let tx = crate::operations::get_transaction_status(provider, &params.transaction_id)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.to_ascii_lowercase().contains("not found") {
                    ErrorData::invalid_params(
                        "Transaction not found for this wallet.".to_string(),
                        None,
                    )
                } else {
                    ErrorData::internal_error(msg, None)
                }
            })?;

        let transaction = build_transaction_record(
            tx.tx_hash.clone(),
            Some(tx.status.clone()),
            tx.privacy_sender.clone(),
            tx.sender.clone(),
            tx.privacy_recipient.clone(),
            tx.recipient.clone(),
            tx.amount.clone(),
            tx.timestamp_ms.unwrap_or(0),
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
            events: tx.events,
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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;
        let privacy_key_guard = self.privacy_key.read().await;
        let privacy_key = privacy_key_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let transactions = crate::operations::get_transactions(provider, ctx, privacy_key)
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

    // deposit tool removed; funding is attempted by createWallet when configured via env

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

        // If prefunded wallets are configured, claim one from the indexer and load it into this
        // session instead of generating/funding on-demand.
        if let Some(prefunded) = self.prefunded_wallets.as_ref() {
            // Ensure provider is configured.
            let provider = self.provider.clone().ok_or_else(|| {
                ErrorData::invalid_params(
                    "Provider not configured. Please set ROLLUP_RPC_URL and INDEXER_URL environment variables.",
                    None,
                )
            })?;

            let claimed = provider
                .claim_prefunded_wallet(self.session_id.as_deref())
                .await
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("Failed to claim prefunded wallet from indexer: {e}"),
                        None,
                    )
                })?;

            let Some(claimed) = claimed else {
                return Err(ErrorData::invalid_params(
                    "No prefunded wallets available. Run the prefund script to generate more wallets.",
                    None,
                ));
            };

            let creds = prefunded.get(&claimed.wallet_address).ok_or_else(|| {
                ErrorData::internal_error(
                    format!(
                        "Indexer returned prefunded wallet {} but it is missing from the configured PREFUNDED_WALLETS_FILE ({:?})",
                        claimed.wallet_address,
                        prefunded.source_path()
                    ),
                    None,
                )
            })?;

            if creds.privacy_address != claimed.privacy_address {
                return Err(ErrorData::internal_error(
                    format!(
                        "Prefunded wallet privacy address mismatch for {}: indexer={}, file={}",
                        claimed.wallet_address, claimed.privacy_address, creds.privacy_address
                    ),
                    None,
                ));
            }

            let wallet_private_key_hex = creds.wallet_private_key_hex.clone();
            let privacy_spend_key_hex = creds.privacy_spend_key_hex.clone();

            let new_wallet_ctx = McpWalletContext::from_private_key_hex(&wallet_private_key_hex)
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("Failed to create wallet context: {}", e),
                        None,
                    )
                })?;
            let wallet_address_str = new_wallet_ctx.get_address().to_string();
            if wallet_address_str != claimed.wallet_address {
                return Err(ErrorData::internal_error(
                    format!(
                        "Prefunded wallet address mismatch: indexer={}, derived={}",
                        claimed.wallet_address, wallet_address_str
                    ),
                    None,
                ));
            }

            let new_privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex).map_err(|e| {
                ErrorData::internal_error(format!("Failed to create privacy key: {}", e), None)
            })?;
            let privacy_address = new_privacy_key.privacy_address(&DOMAIN).to_string();
            if privacy_address != claimed.privacy_address {
                return Err(ErrorData::internal_error(
                    format!(
                        "Prefunded wallet privacy address mismatch: indexer={}, derived={}",
                        claimed.privacy_address, privacy_address
                    ),
                    None,
                ));
            }

            let pool_fvk_pk = std::env::var("POOL_FVK_PK")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|s| parse_hex_32("POOL_FVK_PK", &s))
                .transpose()
                .map_err(|e| {
                    ErrorData::invalid_params(format!("Invalid POOL_FVK_PK: {e}"), None)
                })?;

            let viewer_fvk_bundle = if let Some(pool_pk) = pool_fvk_pk {
                let http = reqwest::Client::new();
                Some(
                    fetch_viewer_fvk_bundle(
                        &http,
                        Some(pool_pk),
                        Some(&privacy_address),
                        Some(&wallet_address_str),
                    )
                    .await
                    .map_err(|e| {
                        ErrorData::internal_error(
                            format!(
                                "Failed to fetch viewer FVK bundle from midnight-fvk-service: {e}"
                            ),
                            None,
                        )
                    })?,
                )
            } else {
                None
            };

            // Replace the wallet context and privacy keys
            let mut ctx_guard = self.wallet_context.write().await;
            *ctx_guard = Some(new_wallet_ctx);

            let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
            *viewer_fvk_guard = viewer_fvk_bundle.clone();

            let mut privacy_key_guard = self.privacy_key.write().await;
            *privacy_key_guard = Some(new_privacy_key);

            // Mark the wallet as explicitly loaded
            let mut loaded_guard = self.wallet_explicitly_loaded.write().await;
            *loaded_guard = true;
            {
                // Reset any cached pending spends from a previous wallet within this session.
                let mut pending = self.pending_spent_notes.lock().await;
                pending.by_rho.clear();
            }
            {
                let mut local = self.local_notes.lock().await;
                local.by_rho.clear();
            }
            self.persist_session_snapshot(SessionSnapshot::from_keys(
                wallet_private_key_hex.clone(),
                privacy_spend_key_hex.clone(),
                viewer_fvk_bundle.clone(),
            ))
            .await;

            tracing::info!("[createWallet] Prefunded wallet claimed successfully");
            tracing::info!("[createWallet] Wallet address: {}", wallet_address_str);
            tracing::info!("[createWallet] Privacy address: {}", privacy_address);

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
            return Ok(CallToolResult::success(vec![Content::text(json)]));
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
        let provider = self.provider.clone().ok_or_else(|| {
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
            Some(
                fetch_viewer_fvk_bundle(
                    &http,
                    Some(pool_pk),
                    Some(&privacy_address),
                    Some(&wallet_address_str),
                )
                .await
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("Failed to fetch viewer FVK bundle from midnight-fvk-service: {e}"),
                        None,
                    )
                })?,
            )
        } else {
            None
        };

        // Auto-fund when configured via AUTO_FUND_DEPOSIT_AMOUNT
        // Flow: Admin sends L2 tokens to new wallet, then new wallet deposits to privacy pool
        if let Some(deposit_amount) = self.auto_fund_deposit_amount {
            let admin_ctx = self.admin_wallet_context.clone().ok_or_else(|| {
                ErrorData::invalid_params(
                    "Auto-fund configured but ADMIN_WALLET_PRIVATE_KEY is not set.",
                    None,
                )
            })?;
            let dest_privacy_key = new_privacy_key_for_deposit.clone();
            let dest_wallet_address = wallet_address_str.clone();
            let new_wallet_for_deposit = new_wallet_for_deposit.clone();
            let auto_fund_gas_reserve = self.auto_fund_gas_reserve;

            run_auto_fund_sequence(
                provider,
                admin_ctx,
                dest_wallet_address,
                dest_privacy_key,
                new_wallet_for_deposit,
                deposit_amount,
                auto_fund_gas_reserve,
            )
            .await
            .map_err(|e| ErrorData::internal_error(format!("Auto-fund failed: {e}"), None))?;
        }

        // Replace the wallet context and privacy keys
        let mut ctx_guard = self.wallet_context.write().await;
        *ctx_guard = Some(new_wallet_ctx);

        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        *viewer_fvk_guard = viewer_fvk_bundle.clone();

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = Some(new_privacy_key);

        // Mark the wallet as explicitly loaded
        let mut loaded_guard = self.wallet_explicitly_loaded.write().await;
        *loaded_guard = true;
        {
            // Reset any cached pending spends from a previous wallet within this session.
            let mut pending = self.pending_spent_notes.lock().await;
            pending.by_rho.clear();
        }
        {
            let mut local = self.local_notes.lock().await;
            local.by_rho.clear();
        }
        self.persist_session_snapshot(SessionSnapshot::from_keys(
            wallet_private_key_hex.clone(),
            privacy_spend_key_hex.clone(),
            viewer_fvk_bundle.clone(),
        ))
        .await;

        tracing::info!("[createWallet] New wallet created successfully");
        tracing::info!("[createWallet] Wallet address: {}", wallet_address_str);
        tracing::info!("[createWallet] Privacy address: {}", privacy_address);

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

                    let commitment =
                        midnight_privacy::fvk_commitment(&midnight_privacy::FullViewingKey(fvk));
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
                        // User-provided FVK doesn't have addresses yet
                        shielded_address: None,
                        wallet_address: None,
                    })
                }
                (None, None) => {
                    let http = reqwest::Client::new();
                    // Note: privacy_address is not known yet at this point in restoreWallet
                    // The FVK service can be updated later via /v1/fvk/:commitment/address
                    Some(
                        fetch_viewer_fvk_bundle(&http, Some(pool_pk), None, None)
                            .await
                            .map_err(|e| {
                                ErrorData::internal_error(
                                    format!(
                                "Failed to fetch viewer FVK bundle from midnight-fvk-service: {e}"
                            ),
                                    None,
                                )
                            })?,
                    )
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

        let viewer_fvk_bundle_for_persist = viewer_fvk_bundle.clone();

        // Replace the existing keys with the restored ones (including wallet context)
        let mut ctx_guard = self.wallet_context.write().await;
        *ctx_guard = Some(new_wallet_ctx);

        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        *viewer_fvk_guard = viewer_fvk_bundle;

        let mut privacy_key_guard = self.privacy_key.write().await;
        *privacy_key_guard = Some(new_privacy_key);

        // Mark the wallet as explicitly loaded
        let mut loaded_guard = self.wallet_explicitly_loaded.write().await;
        *loaded_guard = true;
        {
            // Reset any cached pending spends from a previous wallet within this session.
            let mut pending = self.pending_spent_notes.lock().await;
            pending.by_rho.clear();
        }
        {
            let mut local = self.local_notes.lock().await;
            local.by_rho.clear();
        }
        self.persist_session_snapshot(SessionSnapshot::from_keys(
            wallet_private_key_hex.to_string(),
            privacy_spend_key_hex.to_string(),
            viewer_fvk_bundle_for_persist,
        ))
        .await;

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

    /// Remove the currently loaded wallet for this MCP session.
    /// After calling this, createWallet or restoreWallet can be called again.
    #[tool(
        name = "removeWallet",
        description = "Remove the currently loaded wallet for this MCP session. After calling this, createWallet or restoreWallet can be called again."
    )]
    async fn remove_wallet(
        &self,
        Parameters(_params): Parameters<RemoveWalletRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // This tool is intentionally idempotent: it resets the per-session wallet state and
        // allows createWallet/restoreWallet to be called again.
        let mut wallet_ctx_guard = self.wallet_context.write().await;
        let mut viewer_fvk_guard = self.viewer_fvk_bundle.write().await;
        let mut privacy_key_guard = self.privacy_key.write().await;
        let mut loaded_guard = self.wallet_explicitly_loaded.write().await;

        let was_loaded = *loaded_guard;

        *wallet_ctx_guard = None;
        *viewer_fvk_guard = None;
        *privacy_key_guard = None;
        *loaded_guard = false;
        {
            let mut pending = self.pending_spent_notes.lock().await;
            pending.by_rho.clear();
        }
        {
            let mut local = self.local_notes.lock().await;
            local.by_rho.clear();
        }
        self.clear_session_snapshot().await;

        if was_loaded {
            tracing::info!(
                "[removeWallet] Wallet removed successfully. createWallet and restoreWallet are now available."
            );
        } else {
            tracing::info!(
                "[removeWallet] No wallet to remove. createWallet and restoreWallet are available."
            );
        }

        let result = RemoveWalletResult {
            success: true,
            message: if was_loaded {
                "Wallet removed successfully. You can now use createWallet or restoreWallet."
                    .to_string()
            } else {
                "No wallet to remove. You can now use createWallet or restoreWallet.".to_string()
            },
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

        let privacy_key_guard = self.privacy_key.read().await;
        let privacy_key = privacy_key_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        // Get the current privacy balance to include in status
        let viewer_fvk_guard = self.viewer_fvk_bundle.read().await;
        let viewing_key = viewer_fvk_guard
            .as_ref()
            .map(|bundle| midnight_privacy::FullViewingKey(bundle.fvk));

        let privacy_balance = match crate::operations::get_privacy_balance(
            provider,
            privacy_key,
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
            address: privacy_key.privacy_address(&DOMAIN).to_string(),
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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let addr: PrivacyAddress = params.privacy_address.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid privacy address: {e}"), None)
        })?;

        let res = crate::operations::freeze_address(provider, ctx, addr)
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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let addr: PrivacyAddress = params.privacy_address.parse().map_err(|e| {
            ErrorData::invalid_params(format!("Invalid privacy address: {e}"), None)
        })?;

        let res = crate::operations::unfreeze_address(provider, ctx, addr)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&UnfreezeAddressResult {
            tx_hash: res.tx_hash,
        })
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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let admin: <McpSpec as Spec>::Address = params
            .admin_address
            .parse()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid admin address: {e}"), None))?;

        let res = crate::operations::add_pool_admin(provider, ctx, admin)
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

        let ctx_guard = self.wallet_context.read().await;
        let ctx = ctx_guard.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "No wallet loaded. Call createWallet or restoreWallet first.",
                None,
            )
        })?;

        let admin: <McpSpec as Spec>::Address = params
            .admin_address
            .parse()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid admin address: {e}"), None))?;

        let res = crate::operations::remove_pool_admin(provider, ctx, admin)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&RemovePoolAdminResult {
            tx_hash: res.tx_hash,
        })
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
    if normalized.contains("success")
        || normalized.contains("processed")
        || normalized.contains("finalized")
    {
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
