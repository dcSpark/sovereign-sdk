//! Types for note commitments, nullifiers, and spend proofs

use borsh::{BorshDeserialize, BorshSerialize};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sov_modules_api::macros::UniversalWallet;
use std::fmt;
use std::str::FromStr;

use crate::hash::Hash32;

/// Human-readable prefix for privacy pool addresses
pub const PRIVACY_ADDRESS_HRP: &str = "privpool";

/// A privacy pool address (bech32m-encoded payment address).
/// 
/// This is the user-facing format for privacy recipients. The inner value contains
/// two 32-byte public keys:
/// - `pk_spend` (bytes 0..32): Spending authorization key
/// - `pk_ivk` (bytes 32..64): X25519 public key for incoming note encryption
/// 
/// Both keys are bound into the note recipient address:
///   `recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)`
/// 
/// This binding prevents the "mismatched encryption" attack where a note is
/// committed to one pk_spend but encrypted to a different pk_ivk.
/// 
/// Format: `privpool1<bech32m-encoded-64-bytes>`
/// 
/// # Example
/// ```ignore
/// let addr = PrivacyAddress::new(&pk_spend, &pk_ivk);
/// println!("Send to: {}", addr); // privpool1qypqxpq9qcrsszg2pvxq6rs...
/// 
/// // Parse from string
/// let addr: PrivacyAddress = "privpool1qypqxpq9qcrsszg2pvxq6rs...".parse()?;
/// let (pk_spend, pk_ivk) = addr.keys();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PrivacyAddress {
    /// Spending public key - used to derive note recipient
    pub pk_spend: [u8; 32],
    /// Incoming viewing public key - X25519 key for note encryption
    pub pk_ivk: [u8; 32],
}

impl PrivacyAddress {
    /// Create a PrivacyAddress from both public keys
    pub fn new(pk_spend: &Hash32, pk_ivk: &Hash32) -> Self {
        Self {
            pk_spend: *pk_spend,
            pk_ivk: *pk_ivk,
        }
    }

    /// Get the spending public key (used for recipient derivation)
    pub fn pk_spend(&self) -> &Hash32 {
        &self.pk_spend
    }

    /// Get the incoming viewing public key (used for note encryption)
    pub fn pk_ivk(&self) -> &Hash32 {
        &self.pk_ivk
    }

    /// Get both keys as a tuple
    pub fn keys(&self) -> (&Hash32, &Hash32) {
        (&self.pk_spend, &self.pk_ivk)
    }

    /// Serialize to 64 bytes (pk_spend || pk_ivk)
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&self.pk_spend);
        bytes[32..].copy_from_slice(&self.pk_ivk);
        bytes
    }

    /// Deserialize from 64 bytes
    pub fn from_bytes(bytes: &[u8; 64]) -> Self {
        let mut pk_spend = [0u8; 32];
        let mut pk_ivk = [0u8; 32];
        pk_spend.copy_from_slice(&bytes[..32]);
        pk_ivk.copy_from_slice(&bytes[32..]);
        Self { pk_spend, pk_ivk }
    }
}

impl fmt::Display for PrivacyAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use bech32::{Bech32m, Hrp};
        let hrp = Hrp::parse(PRIVACY_ADDRESS_HRP).expect("valid HRP");
        let bytes = self.to_bytes();
        let encoded = bech32::encode::<Bech32m>(hrp, &bytes).expect("encoding succeeds");
        write!(f, "{}", encoded)
    }
}

impl FromStr for PrivacyAddress {
    type Err = PrivacyAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use bech32::{Bech32m, Hrp};
        
        let (hrp, data) = bech32::decode(s)
            .map_err(|e| PrivacyAddressError::InvalidBech32(e.to_string()))?;
        
        let expected_hrp = Hrp::parse(PRIVACY_ADDRESS_HRP).expect("valid HRP");
        if hrp != expected_hrp {
            return Err(PrivacyAddressError::WrongPrefix {
                expected: PRIVACY_ADDRESS_HRP.to_string(),
                got: hrp.to_string(),
            });
        }
        
        // Verify it's bech32m (not bech32)
        // Re-encode to check variant
        let _: String = bech32::encode::<Bech32m>(hrp, &data)
            .map_err(|_| PrivacyAddressError::NotBech32m)?;
        
        if data.len() != 64 {
            return Err(PrivacyAddressError::WrongLength {
                expected: 64,
                got: data.len(),
            });
        }
        
        let mut bytes = [0u8; 64];
        bytes.copy_from_slice(&data);
        Ok(PrivacyAddress::from_bytes(&bytes))
    }
}

impl Serialize for PrivacyAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PrivacyAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for PrivacyAddress {
    fn schema_name() -> String {
        "PrivacyAddress".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::*;

        let mut obj = SchemaObject::default();
        obj.instance_type = Some(InstanceType::String.into());
        obj.string = Some(Box::new(StringValidation {
            // Pattern for bech32m with 64 bytes of data
            pattern: Some(format!("^{}1[a-z0-9]+$", PRIVACY_ADDRESS_HRP)),
            // Approximate min length: prefix(8) + separator(1) + data(~103) = ~112
            min_length: Some(100),
            ..Default::default()
        }));

        Schema::Object(obj)
    }
}

/// Error type for privacy address parsing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyAddressError {
    /// Invalid bech32 encoding
    InvalidBech32(String),
    /// Wrong human-readable prefix
    WrongPrefix {
        /// Expected prefix
        expected: String,
        /// Actual prefix found
        got: String,
    },
    /// Wrong data length
    WrongLength {
        /// Expected byte length
        expected: usize,
        /// Actual byte length
        got: usize,
    },
    /// Not bech32m variant
    NotBech32m,
}

impl fmt::Display for PrivacyAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBech32(e) => write!(f, "invalid bech32: {}", e),
            Self::WrongPrefix { expected, got } => {
                write!(f, "wrong prefix: expected '{}', got '{}'", expected, got)
            }
            Self::WrongLength { expected, got } => {
                write!(f, "wrong length: expected {} bytes, got {}", expected, got)
            }
            Self::NotBech32m => write!(f, "not bech32m variant (use bech32m, not bech32)"),
        }
    }
}

impl std::error::Error for PrivacyAddressError {}

/// Public data "committed" by the proof package (journal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SpendPublic {
    /// Anchor root used for membership checks.
    pub anchor_root: Hash32,
    /// Nullifiers of the consumed notes (1-4 inputs, PRF-based; no position).
    pub nullifiers: Vec<Hash32>,
    /// For a single native token, the transparent withdrawal amount authorized by the circuit.
    pub withdraw_amount: u128,
    /// Commitments of new shielded outputs (0..=2), in order.
    pub output_commitments: Vec<Hash32>,
    /// Optional viewer attestations (Level B): binds ciphertexts to proof outputs
    pub view_attestations: Option<Vec<ViewAttestation>>,
    /// Optional recipient attestations: binds incoming ciphertexts to outputs.
    /// Each output can have exactly one recipient attestation (for the intended receiver).
    /// The on-chain verifier computes ct_hash from actual ct bytes and verifies it matches.
    pub recipient_attestations: Option<Vec<RecipientAttestation>>,
}

/// A single viewer attestation binding (output_cm, viewer_fvk_commitment, ct_hash, mac).
/// The guest produces these inside the circuit; the module verifies them on-chain.
/// Note: This struct is used internally for verification. Events use the lighter `ViewerBinding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ViewAttestation {
    /// Output commitment this attestation is bound to
    pub cm: Hash32,
    /// FVK commitment: H("FVK_COMMIT_V1" || fvk)
    pub fvk_commitment: Hash32,
    /// Hash of the deterministic ciphertext: H("CT_HASH_V1" || ct)
    pub ct_hash: Hash32,
    /// MAC: H("VIEW_MAC_V1" || k || cm || ct_hash)
    pub mac: Hash32,
}

/// A single recipient attestation binding for incoming note encryption.
/// 
/// This enables receivers to detect and decrypt notes sent to them without
/// leaking information about the recipient on-chain. The proof binds:
/// - `epk`: Ephemeral public key (derived from rho, so deterministic)
/// - `ct_hash`: Hash of the encrypted note plaintext
/// - `mac`: MAC binding the ciphertext to (cm, receiver's pk_ivk)
/// 
/// The on-chain verifier MUST:
/// 1. Read `ct` bytes from the transaction output
/// 2. Compute `ct_hash_chain = H("CT_HASH_V1" || ct)`
/// 3. Verify `ct_hash_chain == ct_hash` from proof
/// 4. Store `(epk, ct, mac)` in the output record for receivers to scan
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize, UniversalWallet)]
pub struct RecipientAttestation {
    /// Output commitment this attestation is bound to
    #[serde(with = "serde_bytes_as_hex_array")]
    pub cm: Hash32,
    /// Ephemeral X25519 public key: X25519_BASE(esk_from_rho_cm(domain, rho, cm))
    #[serde(with = "serde_bytes_as_hex_array")]
    pub epk: Hash32,
    /// Hash of the recipient ciphertext: H("CT_HASH_V1" || ct)
    #[serde(with = "serde_bytes_as_hex_array")]
    pub ct_hash: Hash32,
    /// MAC: H("IN_MAC_V1" || k_in || cm || ct_hash)
    #[serde(with = "serde_bytes_as_hex_array")]
    pub mac: Hash32,
}

impl JsonSchema for RecipientAttestation {
    fn schema_name() -> String {
        "RecipientAttestation".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::*;

        let mut obj = SchemaObject::default();
        obj.instance_type = Some(InstanceType::Object.into());

        let mut properties = std::collections::BTreeMap::new();
        for field in ["cm", "epk", "ct_hash", "mac"] {
            properties.insert(
                field.to_string(),
                Schema::Object(SchemaObject {
                    instance_type: Some(InstanceType::String.into()),
                    format: Some("hex".to_string()),
                    ..Default::default()
                }),
            );
        }

        obj.object = Some(Box::new(ObjectValidation {
            properties,
            required: ["cm", "epk", "ct_hash", "mac"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }));

        Schema::Object(obj)
    }
}

/// Encrypted note for the recipient (incoming encryption).
/// 
/// This is the on-chain data that enables receivers to detect and decrypt notes.
/// The receiver scans by:
/// 1. Compute `dh = X25519(ivk_sk, epk)`
/// 2. If `dh` is all-zero, skip (low-order point)
/// 3. Derive `k_in = H("IN_KDF_V1" || domain || dh || cm)`
/// 4. Decrypt `pt = ct XOR stream(k_in)`
/// 5. Verify `mac == H("IN_MAC_V1" || k_in || cm || H("CT_HASH_V1" || ct))`
/// 6. Parse note from `pt` and verify `cm` matches
/// 7. **CRITICAL**: Verify `recipient_in_note == H("ADDR_V2" || domain || my_pk_spend || my_pk_ivk)`
/// 
/// With ADDR_V2, both pk_spend and pk_ivk are bound into the address. This prevents
/// the "mismatched encryption" attack where an attacker uses your `pk_ivk` but a
/// different `pk_spend`. If keys don't match, the commitment won't match (step 6 fails).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize, UniversalWallet)]
pub struct RecipientCiphertext {
    /// The on-chain commitment this ciphertext is bound to
    #[serde(with = "serde_bytes_as_hex_array")]
    pub cm: Hash32,
    /// Ephemeral X25519 public key for DH key agreement
    #[serde(with = "serde_bytes_as_hex_array")]
    pub epk: Hash32,
    /// Encrypted note plaintext (Poseidon-stream XOR with IN_STREAM_V1)
    pub ct: Vec<u8>,
    /// MAC: H("IN_MAC_V1" || k_in || cm || ct_hash)
    #[serde(with = "serde_bytes_as_hex_array")]
    pub mac: Hash32,
}

impl JsonSchema for RecipientCiphertext {
    fn schema_name() -> String {
        "RecipientCiphertext".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::*;

        let mut obj = SchemaObject::default();
        obj.instance_type = Some(InstanceType::Object.into());

        let mut properties = std::collections::BTreeMap::new();
        for field in ["cm", "epk", "mac"] {
            properties.insert(
                field.to_string(),
                Schema::Object(SchemaObject {
                    instance_type: Some(InstanceType::String.into()),
                    format: Some("hex".to_string()),
                    ..Default::default()
                }),
            );
        }
        properties.insert(
            "ct".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::Array.into()),
                ..Default::default()
            }),
        );

        obj.object = Some(Box::new(ObjectValidation {
            properties,
            required: ["cm", "epk", "ct", "mac"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }));

        Schema::Object(obj)
    }
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
/// Level B: The ciphertext is bound via proof-generated ct_hash and mac, which the
/// module verifies on-chain against the actual ciphertext bytes.
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
    /// The on-chain commitment this ciphertext is bound to.
    #[serde(with = "serde_bytes_as_hex_array")]
    pub cm: Hash32,
    /// XChaCha20-Poly1305 nonce (24 bytes) - kept for backward compat.
    #[serde(with = "serde_bytes_as_hex_array_24")]
    pub nonce: [u8; 24],
    /// Ciphertext bytes (Poseidon-stream XOR for Level B, or XChaCha for legacy).
    pub ct: sov_modules_api::SafeVec<u8, 8_192>,
    /// FVK commitment: H("FVK_COMMIT_V1" || fvk) - binds viewer to ciphertext.
    #[serde(with = "serde_bytes_as_hex_array")]
    pub fvk_commitment: Hash32,
    /// MAC: H("VIEW_MAC_V1" || k || cm || ct_hash) - Level B attestation.
    #[serde(with = "serde_bytes_as_hex_array")]
    pub mac: Hash32,
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
        properties.insert(
            "fvk_commitment".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );
        properties.insert(
            "mac".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(InstanceType::String.into()),
                format: Some("hex".to_string()),
                ..Default::default()
            }),
        );

        obj.object = Some(Box::new(ObjectValidation {
            properties,
            required: vec![
                "cm".to_string(),
                "nonce".to_string(),
                "ct".to_string(),
                "fvk_commitment".to_string(),
                "mac".to_string(),
            ]
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
