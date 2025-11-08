use std::collections::VecDeque;

use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::{Gas, GenesisState, Spec};

use super::ValueMidnightPrivacy;
use crate::hash::{Hash32, RootKey};
use crate::merkle::MerkleTree;

/// Initial configuration for midnight-privacy module.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq, JsonSchema)]
#[schemars(bound = "S: Spec", rename = "MidnightPrivacyConfig")]
pub struct ValueSetterZkConfig<S: Spec> {
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

    /// Gas charged per output appended during epilogue (optional, default zero)
    pub gas_per_output_append: Option<S::Gas>,
}

impl<S: Spec> ValueMidnightPrivacy<S> {
    /// Initializes module with the given configuration.
    pub(crate) fn init_module(
        &mut self,
        config: &<Self as sov_modules_api::Module>::Config,
        state: &mut impl GenesisState<S>,
    ) -> Result<()> {
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

        // Gas for epilogue appends
        let per = config.gas_per_output_append.clone().unwrap_or(<S::Gas as Gas>::zero());
        self.gas_per_output_append.set(&per, state)?;

        // Initialize new parallel-safe pending structures: StateVec and StateMap are empty by default.

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sov_modules_api::prelude::serde_json;
    use sov_modules_api::Spec;
    use sov_test_utils::TestSpec;

    use crate::ValueSetterZkConfig;

    #[test]
    fn test_config_serialization() {
        let admin = <TestSpec as Spec>::Address::from([1; 28]);
        let method_id = [0u8; 32];
        let domain = [0u8; 32];
        let token_id = sov_bank::TokenId::generate::<TestSpec>("test_token");
        let config = ValueSetterZkConfig::<TestSpec> {
            admin,
            method_id,
            tree_depth: 16,
            root_window_size: 100,
            domain,
            token_id,
            gas_per_output_append: None,
        };

        let json_str = serde_json::to_string_pretty(&config).unwrap();
        let parsed_config: ValueSetterZkConfig<TestSpec> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed_config, config);
    }
}
