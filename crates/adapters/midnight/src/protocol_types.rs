use sha2::{Digest, Sha256};

/// Domain separator used for L2 → L1 withdrawal messages.
const DS_L2_TO_L1_WITHDRAW: [u8; 32] = pad_domain("mdn:l2l1:wdraw");
/// Domain separator used for L2 withdrawal tree leaves.
const DS_L2_WITHDRAW_LEAF: [u8; 32] = pad_domain("mdn:l2w:leaf");

/// Zero-filled bytes32 helper reused across hashing routines.
pub const ZERO_BYTES32: [u8; 32] = [0u8; 32];

/// Canonical withdrawal message payload as defined by `ProtocolTypes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawMessage {
    /// L2 sender (compressed as bytes32).
    pub sender: [u8; 32],
    /// L1 recipient (bytes32 address / commitment).
    pub recipient: [u8; 32],
    /// Amount of NIGHT to withdraw.
    pub amount: u128,
    /// Withdrawal nonce assigned by the queue.
    pub nonce: u64,
}

impl WithdrawMessage {
    /// Creates a new withdraw message.
    pub const fn new(sender: [u8; 32], recipient: [u8; 32], amount: u128, nonce: u64) -> Self {
        Self {
            sender,
            recipient,
            amount,
            nonce,
        }
    }
}

/// Compute the canonical SHA-256 hash of an L2 → L1 withdrawal message.
pub fn hash_withdraw_message(message: &WithdrawMessage) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DS_L2_TO_L1_WITHDRAW);
    hasher.update(message.sender);
    hasher.update(message.recipient);
    hasher.update(u128_to_bytes32_le(message.amount));
    hasher.update(u64_to_bytes32_le(message.nonce));
    finalize(hasher)
}

/// Compute the leaf hash derived from a withdrawal message hash.
pub fn hash_withdraw_leaf(message_hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DS_L2_WITHDRAW_LEAF);
    hasher.update(message_hash);
    finalize(hasher)
}

/// Compute an internal Merkle node hash as `SHA256(left || right)`.
pub fn hash_merkle_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    finalize(hasher)
}

/// Encode a `u64` as a 32-byte little-endian value.
pub fn u64_to_bytes32_le(value: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&value.to_le_bytes());
    out
}

/// Encode a `u128` as a 32-byte little-endian value.
pub fn u128_to_bytes32_le(value: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&value.to_le_bytes());
    out
}

const fn pad_domain(tag: &str) -> [u8; 32] {
    let bytes = tag.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

fn finalize(hasher: Sha256) -> [u8; 32] {
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex::ToHex;
    use serde::Deserialize;

    const GOLDEN_VECTORS: &str = include_str!("../test-data/protocol/golden-vectors-v1.json");

    #[derive(Deserialize)]
    struct GoldenVectors {
        vectors: VectorGroup,
        #[serde(rename = "zeroHashes")]
        zero_hashes: ZeroHashes,
    }

    #[derive(Deserialize)]
    struct VectorGroup {
        #[serde(rename = "withdrawal1")]
        withdrawal: WithdrawalVector,
        #[serde(rename = "withdrawLeaf1")]
        withdraw_leaf: WithdrawLeafVector,
    }

    #[derive(Deserialize)]
    struct WithdrawalVector {
        input: WithdrawalInput,
        #[serde(rename = "expectedHash")]
        expected_hash: String,
    }

    #[derive(Deserialize)]
    struct WithdrawalInput {
        #[serde(rename = "senderAscii")]
        sender_ascii: String,
        #[serde(rename = "recipientAscii")]
        recipient_ascii: String,
        amount: String,
        nonce: String,
    }

    #[derive(Deserialize)]
    struct WithdrawLeafVector {
        #[serde(rename = "messageHash")]
        message_hash: String,
        #[serde(rename = "expectedLeafHash")]
        expected_leaf_hash: String,
    }

    #[derive(Deserialize)]
    struct ZeroHashes {
        #[serde(rename = "level0")]
        level0: String,
        #[serde(rename = "level1")]
        level1: String,
    }

    #[test]
    fn withdrawal_hash_matches_golden_vector() {
        let vectors: GoldenVectors = serde_json::from_str(GOLDEN_VECTORS).unwrap();
        let withdrawal = vectors.vectors.withdrawal;
        let msg = WithdrawMessage::new(
            ascii_to_bytes32(&withdrawal.input.sender_ascii),
            ascii_to_bytes32(&withdrawal.input.recipient_ascii),
            withdrawal.input.amount.parse().unwrap(),
            withdrawal.input.nonce.parse().unwrap(),
        );
        let hash = hash_withdraw_message(&msg);
        assert_eq!(hash.encode_hex::<String>(), withdrawal.expected_hash);
    }

    #[test]
    fn withdraw_leaf_hash_matches_vector() {
        let vectors: GoldenVectors = serde_json::from_str(GOLDEN_VECTORS).unwrap();
        let leaf = vectors.vectors.withdraw_leaf;
        let message_hash = decode_hex32(&leaf.message_hash);
        let computed = hash_withdraw_leaf(&message_hash);
        assert_eq!(computed.encode_hex::<String>(), leaf.expected_leaf_hash);
    }

    #[test]
    fn zero_hash_levels_match_contract_reference() {
        let vectors: GoldenVectors = serde_json::from_str(GOLDEN_VECTORS).unwrap();
        let zero_level0 = hash_merkle_node(&ZERO_BYTES32, &ZERO_BYTES32);
        assert_eq!(
            zero_level0.encode_hex::<String>(),
            vectors.zero_hashes.level0
        );
        let zero_level1 = hash_merkle_node(&zero_level0, &zero_level0);
        assert_eq!(
            zero_level1.encode_hex::<String>(),
            vectors.zero_hashes.level1
        );
    }

    fn ascii_to_bytes32(input: &str) -> [u8; 32] {
        let bytes = input.as_bytes();
        assert!(bytes.len() <= 32, "ascii input longer than 32 bytes");
        let mut out = [0u8; 32];
        out[..bytes.len()].copy_from_slice(bytes);
        out
    }

    fn decode_hex32(hex_str: &str) -> [u8; 32] {
        let bytes = hex::decode(hex_str).unwrap();
        assert_eq!(bytes.len(), 32, "hex input must decode to 32 bytes");
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }
}
