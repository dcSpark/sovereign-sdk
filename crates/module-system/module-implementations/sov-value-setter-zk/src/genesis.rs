use anyhow::Result;
use schemars::JsonSchema;
use sov_modules_api::{GenesisState, Spec};

use super::ValueSetterZk;

/// Initial configuration for sov-value-setter-zk module.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq, JsonSchema)]
#[schemars(bound = "S: Spec", rename = "ValueSetterZkConfig")]
pub struct ValueSetterZkConfig<S: Spec> {
    /// Initial value (if any). If not provided, the value will be unset until the first transaction.
    pub initial_value: Option<u32>,

    /// Ligetron method ID (code commitment) of the guest program that verifies value constraints.
    /// This is the SHA-256 hash of (WASM program bytes || packing parameter).
    pub method_id: [u8; 32],

    /// Admin of the module who can update the method ID.
    pub admin: S::Address,
}

impl<S: Spec> ValueSetterZk<S> {
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

        // Set initial value if provided
        if let Some(initial_value) = config.initial_value {
            self.value.set(&initial_value, state)?;
        }

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
            initial_value: Some(42),
        };

        let json_str = serde_json::to_string_pretty(&config).unwrap();
        let parsed_config: ValueSetterZkConfig<TestSpec> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed_config, config);
    }
}
