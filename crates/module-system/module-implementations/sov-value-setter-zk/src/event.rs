use sov_modules_api::macros::serialize;

/// Events emitted by the ValueSetterZk module
#[derive(Debug, PartialEq, Clone, schemars::JsonSchema)]
#[serialize(Borsh, Serde)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// Value was successfully set with proof verification
    ValueSetWithProof {
        /// The value that was set
        value: u32,
    },
    /// Method ID was updated by admin
    MethodIdUpdated {
        /// The new method ID
        new_method_id: [u8; 32],
    },
}
