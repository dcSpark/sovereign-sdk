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
    /// Unmodified data sent from the request body (base64-encoded transaction).
    #[sea_orm(column_type = "Text")]
    pub full_transaction_blob: String,
    /// JSON-serialized proof outputs (e.g., anchor_root, nullifier, withdraw_amount).
    #[sea_orm(column_type = "Text")]
    pub proof_outputs: String,
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
