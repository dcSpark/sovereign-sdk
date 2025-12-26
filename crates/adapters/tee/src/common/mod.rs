use rkyv::{Archive, Deserialize, Serialize};

// Temporary struct to contains the Midnight L2 rollup validation data.
#[derive(Archive, Clone, Debug, Serialize, Deserialize, PartialEq)]
#[rkyv(
    compare(PartialEq),
    derive(Debug),
)]
pub struct AttestationData {
    pub prev_state_root: Vec<u8>,
    pub post_state_root: Vec<u8>,
    pub batch_hash: String,
    pub message_queue_hash: String,
    pub batch_index: u64,
    pub layer2_chain_id: String,
}
