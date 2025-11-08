use sov_modules_api::macros::serialize;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::hash::Hash32;
use crate::types::EncryptedNote;

/// A (commitment, position) pair used by aggregate events.
#[derive(
    Debug,
    PartialEq,
    Clone,
    schemars::JsonSchema,
    serde::Serialize,
    serde::Deserialize,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct CommitmentPos {
    /// The note commitment
    pub commitment: Hash32,
    /// Position in the tree
    pub position: u64,
}

/// Events emitted by the MidnightPrivacy module
#[derive(Debug, PartialEq, Clone, schemars::JsonSchema)]
#[serialize(Borsh, Serde)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    // --- Per-tx events (no positions) ---
    /// Output commitment queued for end-of-block positioning.
    NoteQueued {
        /// The note commitment
        commitment: Hash32,
    },
    /// A note was spent (nullifier consumed)
    NoteSpent {
        /// The nullifier
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
    },
    /// Deposit recorded; positions come later via epilogue.
    PoolDeposit {
        /// Amount deposited
        amount: u128,
        /// The note commitment
        commitment: Hash32,
    },
    /// Pure shielded transfer; outputs are commitments only.
    PoolTransfer {
        /// The nullifier that was spent
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
        /// Output commitments (positions assigned in epilogue)
        outputs: Vec<Hash32>,
    },
    /// Withdrawal; change outputs are commitments only.
    PoolWithdraw {
        /// Amount withdrawn
        amount: u128,
        /// The nullifier
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
        /// Change commitments (positions assigned in epilogue)
        change: Vec<Hash32>,
    },
    // --- Epilogue events ---
    /// Aggregate summary emitted once per block when pending outputs are applied.
    PendingApplied {
        /// Base position for the batch
        base_position: u64,
        /// Number of outputs applied
        count: u64,
        /// New Merkle root after batch
        new_root: Hash32,
    },
    /// Optional detailed mapping for indexers.
    OutputsPositioned {
        /// Commitment-position pairs
        items: Vec<CommitmentPos>,
    },
    // --- Admin/anchor bookkeeping ---
    /// Method ID was updated by admin
    MethodIdUpdated {
        /// The new method ID
        new_method_id: [u8; 32],
    },
    /// A Merkle root was recorded in the permanent historical index (NOMT-backed)
    AnchorRootRecorded {
        /// The root value
        root: Hash32,
        /// Monotonic sequence number (first-seen order)
        seq: u64,
    },
    // --- Back-compat: kept but no longer emitted in new mode ---
    /// Retained for compatibility; not emitted by the new path.
    #[serde(skip)]
    NoteCreated {
        /// The note commitment
        commitment: Hash32,
        /// Position in the tree
        position: u64,
        /// New Merkle root
        new_root: Hash32,
    },
    /// Ciphertext for viewers holding a Full Viewing Key.
    NoteEncrypted {
        /// AEAD-encrypted note bound to its commitment
        enc: EncryptedNote,
    },
}
