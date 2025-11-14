use sov_modules_api::macros::serialize;

use crate::hash::Hash32;
use crate::types::EncryptedNote;

/// Events emitted by the MidnightPrivacy module
#[derive(Debug, PartialEq, Clone, schemars::JsonSchema)]
#[serialize(Borsh, Serde)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// A note commitment was added to the tree
    NoteCreated {
        /// The note commitment
        commitment: Hash32,
        /// Position in the tree
        position: u64,
        /// New Merkle root
        new_root: Hash32,
    },
    /// A note was spent (nullifier consumed)
    NoteSpent {
        /// The nullifier
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
    },
    /// Method ID was updated by admin
    MethodIdUpdated {
        /// The new method ID
        new_method_id: [u8; 32],
    },
    /// Tokens were deposited into the pool and a note was created
    PoolDeposit {
        /// Amount deposited
        amount: u128,
        /// The note commitment
        commitment: Hash32,
        /// Position in the tree
        position: u64,
        /// New Merkle root
        new_root: Hash32,
    },
    /// Tokens were withdrawn from the pool after consuming a nullifier
    PoolWithdraw {
        /// Amount withdrawn
        amount: u128,
        /// The nullifier
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
    },
    /// A Merkle root was recorded in the permanent historical index (NOMT-backed)
    AnchorRootRecorded {
        /// The root value
        root: Hash32,
        /// Monotonic sequence number (first-seen order)
        seq: u64,
    },
    /// Ciphertext for viewers holding a Full Viewing Key.
    /// Viewers will decrypt, recompute the commitment, and compare to `cm`.
    /// This follows Zcash's viewing key pattern: decrypt → recompute → verify.
    NoteEncrypted {
        /// AEAD-encrypted note bound to its commitment
        enc: EncryptedNote,
    },
}
