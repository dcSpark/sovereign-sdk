use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::{GenesisState, Spec};

use super::ValueMidnightPrivacy;
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
        
        // Initialize the commitment tree
        let tree = MerkleTree::new(config.tree_depth);
        self.commitment_tree.set(&tree, state)?;
        
        // Initialize the next position to 0
        self.next_position.set(&0u64, state)?;
        
        // Set the root window size
        self.root_window_size.set(&config.root_window_size, state)?;
        
        // Initialize recent roots with the initial (empty) tree root
        let initial_root = tree.root();
        let roots_vec: Vec<[u8; 32]> = vec![initial_root];
        self.recent_roots.set::<Vec<[u8; 32]>, _>(&roots_vec, state)?;
        
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
        let config = ValueSetterZkConfig::<TestSpec> {
            admin,
            method_id,
            tree_depth: 16,
            root_window_size: 100,
        };

        let json_str = serde_json::to_string_pretty(&config).unwrap();
        let parsed_config: ValueSetterZkConfig<TestSpec> = 
            serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed_config, config);
    }
}

