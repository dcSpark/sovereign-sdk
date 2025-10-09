//! Types for note commitments, nullifiers, and spend proofs

use borsh::{BorshDeserialize, BorshSerialize};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sov_rollup_interface::sov_universal_wallet::schema::OverrideSchema;

use crate::hash::Hash32;

/// Public data "committed" by the proof package (journal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SpendPublic {
    /// Anchor root used for membership checks.
    pub anchor_root: Hash32,
    /// Nullifier of the consumed note.
    pub nullifier: Hash32,
}

/// Witness for a single-input spend (simple demo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SpendWitness {
    /// Tree depth.
    pub tree_depth: u8,
    /// Domain tag.
    pub domain: Hash32,
    /// Anchor root.
    pub anchor_root: Hash32,

    /// Note commitment.
    pub cm: Hash32,
    /// Secret nf key to derive the nullifier.
    pub nf_key: Hash32,

    /// Leaf index.
    pub pos: u64,
    /// Merkle path (bottom-up), length == `tree_depth`.
    pub siblings: Vec<Hash32>,
}

/// A note stored in the commitment tree
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Note {
    /// Domain tag for the note
    #[serde(with = "serde_bytes_as_hex_array")]
    pub domain: Hash32,
    /// Value of the note
    pub value: u128,
    /// Random nonce
    #[serde(with = "serde_bytes_as_hex_array")]
    pub rho: Hash32,
    /// Recipient binding
    #[serde(with = "serde_bytes_as_hex_array")]
    pub recipient: Hash32,
}

// Implement JsonSchema manually for Note
impl JsonSchema for Note {
    fn schema_name() -> String {
        "Note".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::*;
        
        let mut obj = SchemaObject::default();
        obj.instance_type = Some(InstanceType::Object.into());
        
        let mut properties = std::collections::BTreeMap::new();
        properties.insert("domain".to_string(), Schema::Object(SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            format: Some("hex".to_string()),
            ..Default::default()
        }));
        properties.insert("value".to_string(), Schema::Object(SchemaObject {
            instance_type: Some(InstanceType::Integer.into()),
            ..Default::default()
        }));
        properties.insert("rho".to_string(), Schema::Object(SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            format: Some("hex".to_string()),
            ..Default::default()
        }));
        properties.insert("recipient".to_string(), Schema::Object(SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            format: Some("hex".to_string()),
            ..Default::default()
        }));
        
        obj.object = Some(Box::new(ObjectValidation {
            properties,
            required: vec!["domain".to_string(), "value".to_string(), "rho".to_string(), "recipient".to_string()].into_iter().collect(),
            ..Default::default()
        }));
        
        Schema::Object(obj)
    }
}

// Implement OverrideSchema for Note to make it compatible with UniversalWallet
impl OverrideSchema for Note {
    type Output = Note;
}

// Custom serde module for hex-encoded byte arrays
mod serde_bytes_as_hex_array {
    use serde::{Deserialize, Deserializer, Serializer};
    
    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }
    
    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom("Expected 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }
}

