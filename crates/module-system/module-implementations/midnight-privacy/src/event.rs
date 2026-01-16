use sov_modules_api::macros::serialize;
use sov_modules_api::Spec;

use crate::hash::Hash32;
use crate::types::{EncryptedNote, PrivacyAddress};

/// Lightweight viewer attestation for events.
/// Contains only the essential information for indexers/authorities.
/// The full ct_hash and mac were already verified on-chain; no need to include them in events.
#[derive(Debug, PartialEq, Clone, schemars::JsonSchema)]
#[serialize(Borsh, Serde)]
pub struct ViewerBinding {
    /// Output commitment this attestation is bound to
    pub cm: Hash32,
    /// FVK commitment identifying which viewer key holder can decrypt this note.
    /// H("FVK_COMMIT_V1" || fvk)
    pub fvk_commitment: Hash32,
}

/// Events emitted by the MidnightPrivacy module.
///
/// **IMPORTANT: Position and root values in per-tx events are PROVISIONAL.**
///
/// Due to parallel execution, the actual tree positions and roots are only finalized
/// at the end of the block in `end_block_flush`. Events emitted during transaction
/// execution contain placeholder values:
/// - `position`: Will be `None` (unknown until flush)
/// - `new_root`: Will be `[0u8; 32]` placeholder
///
/// For authoritative position/root data, indexers should:
/// 1. Query `/modules/midnight-privacy/notes` endpoint after block finalization
/// 2. Use `AnchorRootRecorded` events which are emitted during flush with real roots
#[derive(Debug, PartialEq, Clone, schemars::JsonSchema)]
#[serialize(Borsh, Serde)]
#[serde(bound = "S: Spec", rename_all = "snake_case")]
#[schemars(bound = "S: Spec", rename = "Event")]
pub enum Event<S: Spec> {
    /// A note commitment was queued for addition to the tree.
    ///
    /// **Note:** Position is provisional and will be assigned at end-of-block flush.
    /// The `new_root` is a placeholder `[0u8; 32]` - real roots come from `AnchorRootRecorded`.
    NoteCreated {
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
    /// Method ID was updated by admin
    MethodIdUpdated {
        /// The new method ID
        new_method_id: [u8; 32],
    },
    /// Tokens were deposited into the pool and a note was queued for creation.
    ///
    /// **Note:** Position is provisional. Final position assigned at end-of-block flush.
    PoolDeposit {
        /// Amount deposited
        amount: u128,
        /// The note commitment
        commitment: Hash32,
    },
    /// Shielded → Shielded transfer (pure privacy).
    ///
    /// **Note:** Output positions are provisional. Final positions assigned at flush.
    PoolTransfer {
        /// The nullifier that was spent
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
        /// Output note commitments added by this transfer
        outputs: Vec<Hash32>,
        /// Viewer bindings from the proof (Level-B compliance).
        /// Present when the prover included viewer attestations binding ciphertexts to outputs.
        /// Each binding indicates which viewer (by fvk_commitment) can decrypt a specific output (by cm).
        viewer_bindings: Option<Vec<ViewerBinding>>,
    },
    /// Tokens were withdrawn from the pool after consuming a nullifier.
    ///
    /// **Note:** Change output positions are provisional.
    PoolWithdraw {
        /// Amount withdrawn
        amount: u128,
        /// The nullifier
        nullifier: Hash32,
        /// The anchor root used
        anchor_root: Hash32,
        /// Change output commitments created (often 0 or 1)
        change: Vec<Hash32>,
        /// Viewer bindings from the proof (Level-B compliance).
        /// Present when the prover included viewer attestations binding ciphertexts to change outputs.
        /// Each binding indicates which viewer (by fvk_commitment) can decrypt a specific output (by cm).
        viewer_bindings: Option<Vec<ViewerBinding>>,
    },
    /// A Merkle root was recorded in the permanent historical index (NOMT-backed).
    ///
    /// This event is emitted during `end_block_flush` and contains the **authoritative**
    /// root value. Use this for anchor tracking, not per-tx events.
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

    // === Deny-map (freeze/blacklist) events ===
    //
    // IMPORTANT: These variants are intentionally appended at the end of the enum to preserve
    // the discriminant indices of previously-emitted events under Borsh serialization.
    /// Deny-map root (blacklist) was updated by a pool admin.
    BlacklistRootUpdated {
        /// Previous root.
        old_blacklist_root: Hash32,
        /// New root.
        new_blacklist_root: Hash32,
    },
    /// A privacy address was frozen (blacklisted) by a pool admin.
    AddressFrozen {
        /// The user-facing privacy address.
        address: PrivacyAddress,
        /// The internal recipient identifier used as the deny-map key.
        recipient: Hash32,
    },
    /// A privacy address was unfrozen (un-blacklisted) by a pool admin.
    AddressUnfrozen {
        /// The user-facing privacy address.
        address: PrivacyAddress,
        /// The internal recipient identifier used as the deny-map key.
        recipient: Hash32,
    },
    /// A pool admin was added by the module admin.
    PoolAdminAdded {
        /// The admin address that was added.
        admin: S::Address,
    },
    /// A pool admin was removed by the module admin.
    PoolAdminRemoved {
        /// The admin address that was removed.
        admin: S::Address,
    },
}
