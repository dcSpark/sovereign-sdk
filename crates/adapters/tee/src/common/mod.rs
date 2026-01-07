use alloy_primitives::U256;
use borsh::{BorshDeserialize, BorshSerialize};

// Batch public data
#[derive(Clone, Debug, PartialEq, BorshSerialize, BorshDeserialize, Eq)]
pub struct BatchPublicDataV1 {
    pub version: u32,
    pub layer2_chain_id: u64,
    pub batch_index: u64,

    pub da_start_height: u64,
    pub da_end_height: u64,
    pub da_commitment: [u8; 32],

    pub prev_state_root: [u8; 64],
    pub post_state_root: [u8; 64],

    pub prev_batch_hash: [u8; 32],
    pub batch_hash: [u8; 32],

    // Unsigned integer 256
    // Not using ethereum_types as we need to serialize it with Borsh...
    pub last_processed_queue_index: U256,
    pub message_queue_hash: [u8; 32],
    pub withdraw_root: [u8; 32],
}

impl Default for BatchPublicDataV1 {
    fn default() -> Self {
        Self {
            version: 1,
            layer2_chain_id: 0,
            batch_index: 0,
            da_start_height: 0,
            da_end_height: 0,
            da_commitment: [0u8; 32],
            prev_state_root: [0u8; 64],
            post_state_root: [0u8; 64],
            prev_batch_hash: [0u8; 32],
            batch_hash: [0u8; 32],
            last_processed_queue_index: U256::ZERO,
            message_queue_hash: [0u8; 32],
            withdraw_root: [0u8; 32],
        }
    }
}
