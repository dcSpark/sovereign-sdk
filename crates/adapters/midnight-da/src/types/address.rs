use std::str::FromStr;

use sov_rollup_interface::crypto::CredentialId;
use sov_rollup_interface::sov_universal_wallet::UniversalWallet;
use sov_rollup_interface::BasicAddress;

/// Sequencer DA address used in tests.
pub const MOCK_SEQUENCER_DA_ADDRESS: [u8; 32] = [0u8; 32];

/// A mock address type used for testing. Internally, this type is standard 32 byte array.
#[derive(
    Debug,
    Copy,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    borsh::BorshDeserialize,
    borsh::BorshSerialize,
)]
#[cfg_attr(
    feature = "arbitrary",
    derive(arbitrary::Arbitrary, proptest_derive::Arbitrary)
)]
pub struct MidnightAddress {
    /// Underlying mock address.
    addr: [u8; 32],
}

// Serialize MidnightAddress without field labels. This changes the output from `{ addr: 0x0000000000000000000000000000000000000000000000}`
// to just `0x0000000000000000000000000000000000000000000000`
#[derive(UniversalWallet)]
#[allow(dead_code)]
#[doc(hidden)]
pub struct MidnightAddressSchema(#[sov_wallet(display(hex))] [u8; 32]);
impl sov_rollup_interface::sov_universal_wallet::schema::OverrideSchema for MidnightAddress {
    type Output = MidnightAddressSchema;
}

impl MidnightAddress {
    /// Creates a new mock address containing the given bytes.
    pub const fn new(addr: [u8; 32]) -> Self {
        Self { addr }
    }
}

impl schemars::JsonSchema for MidnightAddress {
    fn schema_name() -> String {
        "MidnightAddress".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        serde_json::from_value(serde_json::json!({
            "type": "string",
            "pattern": "^[a-fA-F0-9]{64}$",
            // This description assumes that `serializer` uses a human-readable format.
            "description": "Midnight address; 32 bytes in hex-encoded format",
        }))
        .unwrap()
    }
}

impl serde::Serialize for MidnightAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            hex::serialize(self.addr, serializer)
        } else {
            self.addr.serialize(serializer)
        }
    }
}

impl<'de> serde::Deserialize<'de> for MidnightAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let string: String = serde::Deserialize::deserialize(deserializer)?;
            Self::from_str(&string).map_err(serde::de::Error::custom)
        } else {
            serde::Deserialize::deserialize(deserializer).map(MidnightAddress::new)
        }
    }
}

impl FromStr for MidnightAddress {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let addr = hex::decode(s.strip_prefix("0x").unwrap_or(s)).map_err(anyhow::Error::msg)?;
        Self::try_from(addr.as_slice())
    }
}

impl<'a> TryFrom<&'a [u8]> for MidnightAddress {
    type Error = anyhow::Error;

    fn try_from(addr: &'a [u8]) -> Result<Self, Self::Error> {
        let addr = addr
            .try_into()
            .map_err(|_| anyhow::anyhow!("address must be 32 bytes long"))?;
        Ok(Self { addr })
    }
}

impl AsRef<[u8]> for MidnightAddress {
    fn as_ref(&self) -> &[u8] {
        &self.addr
    }
}

impl From<[u8; 32]> for MidnightAddress {
    fn from(addr: [u8; 32]) -> Self {
        MidnightAddress { addr }
    }
}

impl std::fmt::Display for MidnightAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "0x{}", hex::encode(self.addr))
    }
}

impl BasicAddress for MidnightAddress {}

impl From<CredentialId> for MidnightAddress {
    fn from(credential_id: CredentialId) -> Self {
        MidnightAddress {
            addr: credential_id.0 .0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use proptest::prelude::any;
    use proptest::proptest;
    use sov_rollup_interface::sov_universal_wallet::schema::Schema;
    use sov_test_utils::validate_schema;

    use super::*;

    #[test]
    fn human_readable_serde_roundtrip() {
        let addr = MidnightAddress::new([3u8; 32]);
        let json = serde_json::to_string(&addr).unwrap();
        let recovered_addr = serde_json::from_str::<MidnightAddress>(&json).unwrap();
        assert_eq!(addr, recovered_addr);
    }

    #[test]
    fn universal_wallet_roundtrip() {
        let addr = MidnightAddress::new([3u8; 32]);
        let serialized = borsh::to_vec(&addr).unwrap();
        let schema = Schema::of_single_type::<MidnightAddress>().unwrap();

        assert_eq!(schema.display(0, &serialized).unwrap(), addr.to_string());
    }

    #[test]
    fn binary_serde_roundtrip() {
        let addr = MidnightAddress::new([3u8; 32]);
        let bytes = bincode::serialize(&addr).unwrap();
        let recovered_addr = bincode::deserialize::<MidnightAddress>(&bytes).unwrap();
        assert_eq!(addr, recovered_addr);
    }

    #[test]
    fn try_from_bytes() {
        let addr = MidnightAddress::new([100u8; 32]);
        let addr_bytes = addr.as_ref();
        let recovered_addr = MidnightAddress::try_from(addr_bytes).unwrap();
        assert_eq!(addr, recovered_addr);
    }

    #[test]
    fn parse_from_string() {
        let addr = MidnightAddress::new([1u8; 32]);
        let s = addr.to_string();
        let recovered_addr = s.parse::<MidnightAddress>().unwrap();
        assert_eq!(addr, recovered_addr);
    }

    proptest! {
        #[test]
        fn json_schema_is_valid(item in any::<MidnightAddress>()) {
            validate_schema(&item).unwrap();
        }
    }
}
