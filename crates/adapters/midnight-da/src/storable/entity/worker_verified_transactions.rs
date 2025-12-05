//! Database model storing transactions that were verified off-chain by the proof
//! verifier service before being handed to the rollup.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Transaction state in the processing pipeline
#[derive(Debug, Clone, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::None)")]
pub enum TransactionState {
    /// Accepted by the verifier service, but not yet processed by the sequencer
    #[sea_orm(string_value = "pending")]
    Pending,
    /// Accepted by the sequencer
    #[sea_orm(string_value = "accepted")]
    Accepted,
    /// Rejected by the sequencer
    #[sea_orm(string_value = "rejected")]
    Rejected,
}

/// Row representing the verification outcome for a single rollup transaction.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "worker_verified_transactions")]
pub struct Model {
    /// Surrogate primary key.
    #[sea_orm(primary_key, auto_increment = true, column_type = "Integer")]
    pub id: i32,
    /// Hex-encoded transaction hash (prefixed with `0x`).
    pub tx_hash: String,
    /// Whether the transaction signature verified successfully.
    pub signature_valid: bool,
    /// Whether the zero-knowledge proof verified successfully.
    /// - `Some(true)`: has proof and verified correctly
    /// - `Some(false)`: has proof but verification failed
    /// - `None`: transaction doesn't have a proof (e.g., deposits)
    #[sea_orm(nullable)]
    pub proof_verified: Option<bool>,
    /// JSON representation of the transaction call message with proof replaced by "REMOVED".
    #[sea_orm(column_type = "Text")]
    pub transaction_data: String,
    /// JSON-serialized proof outputs (e.g., anchor_root, nullifier, withdraw_amount).
    #[sea_orm(column_type = "Text")]
    pub proof_outputs: String,
    /// JSON-serialized Full Viewing Keys attached to a deposit (if any).
    #[sea_orm(column_type = "Text", nullable)]
    pub view_fvks_json: Option<String>,
    /// JSON-serialized viewer attestations from the proof (if any).
    #[sea_orm(column_type = "Text", nullable)]
    pub view_attestations_json: Option<String>,
    /// Borsh-serialized public key (hex string) - for pre-authenticated path
    #[sea_orm(column_type = "Text", nullable)]
    pub pub_key_hex: Option<String>,
    /// Borsh-serialized signature (hex string) - for pre-authenticated path
    #[sea_orm(column_type = "Text", nullable)]
    pub signature_hex: Option<String>,
    /// Borsh-serialized uniqueness data (hex string) - for pre-authenticated path
    #[sea_orm(column_type = "Text", nullable)]
    pub uniqueness_hex: Option<String>,
    /// Borsh-serialized transaction details (hex string) - for pre-authenticated path
    #[sea_orm(column_type = "Text", nullable)]
    pub details_hex: Option<String>,
    /// Borsh-serialized runtime call message (hex string) - for pre-authenticated path
    #[sea_orm(column_type = "Text", nullable)]
    pub runtime_call_hex: Option<String>,
    /// Fully serialized transaction (base64) - optimized pre-authenticated path
    /// Contains the complete borsh-serialized Transaction, ready to wrap and authenticate
    #[sea_orm(column_type = "Text", nullable)]
    pub serialized_tx_base64: Option<String>,
    /// L2 sender address (canonical string), derived from the transaction's public key.
    #[sea_orm(column_type = "String(StringLen::None)")]
    pub sender: String,
    /// Withdraw recipient (transparent address), if present in the transaction.
    #[sea_orm(column_type = "Text", nullable)]
    pub recipient: Option<String>,
    /// Current state of the transaction in the processing pipeline.
    pub transaction_state: TransactionState,
    /// Response from the sequencer after processing (JSON or error message).
    #[sea_orm(column_type = "Text", nullable)]
    pub sequencer_status: Option<String>,
    /// Timestamp indicating when the record was written.
    pub created_at: DateTimeUtc,
}

/// No relations are defined for this table.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
