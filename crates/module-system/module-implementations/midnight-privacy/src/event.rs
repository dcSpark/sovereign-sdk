use sov_modules_api::macros::serialize;

use crate::hash::Hash32;

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
}

