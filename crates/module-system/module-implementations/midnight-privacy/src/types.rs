//! Types for note commitments, nullifiers, and spend proofs

use borsh::{BorshDeserialize, BorshSerialize};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sov_modules_api::macros::UniversalWallet;

use crate::hash::Hash32;

/// Public data "committed" by the proof package (journal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SpendPublic {
    /// Anchor root used for membership checks.
    pub anchor_root: Hash32,
    /// Nullifier of the consumed note (PRF-based; no position).
    pub nullifier: Hash32,
    /// For a single native token, the transparent withdrawal amount authorized by the circuit.
    pub withdraw_amount: u128,
    /// Commitments of new shielded outputs (0..=2), in order.
    pub output_commitments: Vec<Hash32>,
}

/// Witness for a single-input spend (simple demo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SpendWitness {
    /// Tree depth.
    pub tree_depth: u8,
    /// Domain tag (from chain state).
    pub domain: Hash32,
    /// Anchor root.
    pub anchor_root: Hash32,

    // --- Note opening (private) ---
    /// Value of the note.
    pub value: u128,
    /// Random nonce (used in both commitment and nullifier).
    pub rho: Hash32,
    /// Recipient binding.
    pub recipient: Hash32,
    /// Secret nf key to derive the nullifier.
    pub nf_key: Hash32,

    // --- Merkle membership ---
    /// Leaf index.
    pub pos: u64,
    /// Merkle path (bottom-up), length == `tree_depth`.
    pub siblings: Vec<Hash32>,

    // --- Withdrawal binding (public in journal) ---
    /// Withdrawal amount authorized by this proof.
    pub withdraw_amount: u128,
}

/// A note stored in the commitment tree
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    UniversalWallet,
)]
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
        properties.insert(
            "domain".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );
        properties.insert(
            "value".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::Integer.into()),
                ..Default::default()
            }),
        );
        properties.insert(
            "rho".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );
        properties.insert(
            "recipient".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );

        obj.object = Some(Box::new(ObjectValidation {
            properties,
            required: vec![
                "domain".to_string(),
                "value".to_string(),
                "rho".to_string(),
                "recipient".to_string(),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }));

        Schema::Object(obj)
    }
}

// Custom serde module for hex-encoded byte arrays
pub(crate) mod serde_bytes_as_hex_array {
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

// --- Viewing key + encrypted note types ---

/// A 32-byte Full Viewing Key (FVK) that allows decrypting notes for auditing.
/// Share this with a viewer; keep it confidential like any other secret key.
///
/// This design follows Zcash's viewing key pattern: viewers can decrypt notes
/// and recompute the commitment to verify truthfulness against the on-chain commitment.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    UniversalWallet,
)]
pub struct FullViewingKey(#[serde(with = "serde_bytes_as_hex_array")] pub [u8; 32]);

impl JsonSchema for FullViewingKey {
    fn schema_name() -> String {
        "FullViewingKey".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::*;

        let mut obj = SchemaObject::default();
        obj.instance_type = Some(InstanceType::String.into());
        obj.format = Some("hex".to_string());

        Schema::Object(obj)
    }
}

/// AEAD-encrypted note bound to its on-chain commitment.
/// A viewer uses FVK to decrypt and then recomputes cm to verify truthfulness.
///
/// The ciphertext is bound to the commitment via AEAD AAD, preventing "trust me bro"
/// scenarios. The viewer must recompute the commitment from the decrypted note and
/// verify it matches the on-chain commitment.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    UniversalWallet,
)]
pub struct EncryptedNote {
    /// The on-chain commitment this ciphertext is bound to (also used as AEAD AAD).
    #[serde(with = "serde_bytes_as_hex_array")]
    pub cm: Hash32,
    /// XChaCha20-Poly1305 nonce (24 bytes).
    #[serde(with = "serde_bytes_as_hex_array_24")]
    pub nonce: [u8; 24],
    /// Ciphertext bytes (AEAD).
    pub ct: sov_modules_api::SafeVec<u8, 8_192>,
}

impl JsonSchema for EncryptedNote {
    fn schema_name() -> String {
        "EncryptedNote".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::*;

        let mut obj = SchemaObject::default();
        obj.instance_type = Some(InstanceType::Object.into());

        let mut properties = std::collections::BTreeMap::new();
        properties.insert(
            "cm".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );
        properties.insert(
            "nonce".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );
        properties.insert(
            "ct".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::Array.into()),
                ..Default::default()
            }),
        );

        obj.object = Some(Box::new(ObjectValidation {
            properties,
            required: vec!["cm".to_string(), "nonce".to_string(), "ct".to_string()]
                .into_iter()
                .collect(),
            ..Default::default()
        }));

        Schema::Object(obj)
    }
}

// Custom serde module for 24-byte nonce
mod serde_bytes_as_hex_array_24 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 24], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 24], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 24 {
            return Err(serde::de::Error::custom("Expected 24 bytes"));
        }
        let mut arr = [0u8; 24];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }
}
