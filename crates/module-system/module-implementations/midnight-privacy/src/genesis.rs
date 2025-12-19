use std::collections::VecDeque;

use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::{GenesisState, Spec};

use super::ValueMidnightPrivacy;
use crate::hash::{Hash32, RootKey};
use crate::merkle::{MerkleTree, MAX_TREE_DEPTH};

/// Initial configuration for midnight-privacy module.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq, JsonSchema)]
#[schemars(bound = "S: Spec", rename = "MidnightPrivacyConfig")]
pub struct MidnightPrivacyConfig<S: Spec> {
    /// Depth of the commitment tree (tree will have 2^depth leaves)
    pub tree_depth: u8,

    /// Size of the recent roots window (how many recent roots to keep)
    pub root_window_size: u32,

    /// Ligero method ID (code commitment) of the guest program that verifies spend proofs.
    /// This is the SHA-256 hash of (WASM program bytes || packing parameter).
    pub method_id: [u8; 32],

    /// Admin of the module who can update the method ID.
    pub admin: S::Address,

    /// Domain tag used in all note/hash derivations
    pub domain: Hash32,

    /// Single supported token (native)
    pub token_id: sov_bank::TokenId,
}

impl<S: Spec> ValueMidnightPrivacy<S> {
    /// Initializes module with the given configuration.
    pub(crate) fn init_module(
        &mut self,
        config: &<Self as sov_modules_api::Module>::Config,
        state: &mut impl GenesisState<S>,
    ) -> Result<()> {
        // Sanity check: tree_depth must not exceed what the guest circuit supports.
        // The guest uses `1u64 << depth` for position bounds, so depth must be ≤ 63.
        anyhow::ensure!(
            config.tree_depth <= MAX_TREE_DEPTH,
            "midnight-privacy: tree_depth {} exceeds MAX_TREE_DEPTH {}",
            config.tree_depth,
            MAX_TREE_DEPTH,
        );

        // Set the admin
        self.admin.set(&config.admin, state)?;

        // Set the method ID
        self.method_id.set(&config.method_id, state)?;

        // New: bind domain + native token in state
        self.domain.set(&config.domain, state)?;
        self.token_id.set(&config.token_id, state)?;

        // Initialize the commitment tree
        let tree = MerkleTree::new(config.tree_depth);
        self.commitment_tree.set(&tree, state)?;

        // Initialize the next position to 0
        self.next_position.set(&0u64, state)?;

        // Initialize the nullifier tree (Aztec-style dual-tree design)
        // Uses the same initial depth as commitment tree; both can grow dynamically.
        let nf_tree = MerkleTree::new(config.tree_depth);
        self.nullifier_tree.set(&nf_tree, state)?;
        self.next_nullifier_position.set(&0u64, state)?;

        // Set the root window size
        self.root_window_size.set(&config.root_window_size, state)?;

        // Initialize recent roots with the initial (empty) tree root
        let initial_root = tree.root();
        let mut roots_deque = VecDeque::new();
        roots_deque.push_back(initial_root);
        self.recent_roots.set(&roots_deque, state)?;

        // Initialize full-history root index (NOMT-backed for permanent storage)
        // The initial empty-tree root is assigned sequence 0
        self.root_seq.set(&1u64, state)?; // next seq to use
        self.all_roots.set(&RootKey(initial_root), &0u64, state)?;

        // Initialize statistics counters
        self.total_deposited.set(&0u128, state)?;
        self.deposit_count.set(&0u64, state)?;
        self.total_withdrawn.set(&0u128, state)?;
        self.withdraw_count.set(&0u64, state)?;
        self.spent_nullifier_count.set(&0u64, state)?;

        // Initialize indexed pending roots counter for genesis height (no roots yet)
        // Note: StateMap doesn't require explicit initialization, but we set 0 for clarity
        // let height = state.rollup_height_to_access(); // Would be RollupHeight::GENESIS
        // self.pending_roots_count.set(&height, &0u32, state)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sov_modules_api::prelude::serde_json;
    use sov_modules_api::Spec;
    use sov_test_utils::TestSpec;

    use crate::MidnightPrivacyConfig;

    #[test]
    fn test_config_serialization() {
        let admin = <TestSpec as Spec>::Address::from([1; 28]);
        let method_id = [0u8; 32];
        let domain = [0u8; 32];
        let token_id = sov_bank::TokenId::generate::<TestSpec>("test_token");
        let config = MidnightPrivacyConfig::<TestSpec> {
            admin,
            method_id,
            tree_depth: 16,
            root_window_size: 100,
            domain,
            token_id,
        };

        let json_str = serde_json::to_string_pretty(&config).unwrap();
        let parsed_config: MidnightPrivacyConfig<TestSpec> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed_config, config);
    }
}
