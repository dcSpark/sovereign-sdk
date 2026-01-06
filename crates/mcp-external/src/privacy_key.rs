//! Privacy Key Management (Spending Secret Key)
//!
//! This module provides functionality for managing the privacy spending secret key,
//! which is used to derive the public key, recipient addresses, and nullifier keys
//! for privacy pool operations.
//!
//! The spending secret key (spend_sk) enables:
//! - Deriving public key: pk = H("PK_V1" || spend_sk)
//! - Deriving recipient: recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)
//! - Deriving nullifier key: nf_key = H("NFKEY_V1" || domain || spend_sk)

use anyhow::{Context, Result};
use midnight_privacy::{nf_key_from_sk, pk_from_sk, recipient_from_pk, Hash32, PrivacyAddress};

/// Privacy key context that manages the spending secret key for privacy operations
///
/// The spending secret key is a 32-byte key that can be used to:
/// - Receive funds (via derived public key and recipient address)
/// - Spend notes (via derived nullifier key)
#[derive(Clone)]
pub struct PrivacyKey {
    /// The 32-byte spending secret key
    spend_sk: Hash32,
    /// Cached public key derived from spend_sk
    pk: Hash32,
}

impl PrivacyKey {
    /// Create a PrivacyKey from a hex string
    ///
    /// # Parameters
    /// * `spend_sk_hex` - Spending secret key as hex string (with or without "0x" prefix)
    pub fn from_hex(spend_sk_hex: impl AsRef<str>) -> Result<Self> {
        let hex_str = spend_sk_hex.as_ref().trim();
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);

        // Decode hex to bytes
        let spend_sk_bytes =
            hex::decode(hex_str).context("Failed to decode spending secret key hex string")?;

        if spend_sk_bytes.len() != 32 {
            anyhow::bail!(
                "Spending secret key must be exactly 32 bytes (64 hex characters), got {} bytes",
                spend_sk_bytes.len()
            );
        }

        let mut spend_sk = [0u8; 32];
        spend_sk.copy_from_slice(&spend_sk_bytes);

        // Derive public key once during initialization
        let pk = pk_from_sk(&spend_sk);

        tracing::info!("Privacy key initialized");
        tracing::debug!("Spend SK: 0x{}", hex::encode(&spend_sk));
        tracing::debug!("Derived PK: 0x{}", hex::encode(&pk));

        Ok(Self { spend_sk, pk })
    }

    /// Create a PrivacyKey from a bech32m privacy address string
    ///
    /// Note: This only stores the public key (pk_out), not the spending secret key.
    /// With only the public key, you can:
    /// - Derive recipient addresses (for receiving)
    /// - NOT spend notes (requires spend_sk)
    ///
    /// # Parameters
    /// * `address` - Privacy address in bech32m format (e.g., "privpool1...")
    pub fn from_address(address: impl AsRef<str>) -> Result<Self> {
        let privacy_addr: PrivacyAddress = address
            .as_ref()
            .parse()
            .context("Failed to parse privacy address")?;

        let pk = privacy_addr.to_pk();

        tracing::info!("Privacy key initialized from address (public key only)");
        tracing::debug!("PK: 0x{}", hex::encode(&pk));
        tracing::warn!("Note: Without spend_sk, spending operations will not be possible");

        Ok(Self {
            spend_sk: [0u8; 32], // Zero spend_sk when created from address
            pk,
        })
    }

    /// Get the raw spending secret key bytes
    ///
    /// Returns None if this PrivacyKey was created from an address (public key only)
    pub fn spend_sk(&self) -> Option<&Hash32> {
        // Check if spend_sk is all zeros (created from address)
        if self.spend_sk == [0u8; 32] {
            None
        } else {
            Some(&self.spend_sk)
        }
    }

    /// Get the derived public key
    #[allow(dead_code)]
    pub fn pk(&self) -> &Hash32 {
        &self.pk
    }

    /// Get the privacy address (bech32m format)
    pub fn privacy_address(&self) -> PrivacyAddress {
        PrivacyAddress::from_pk(&self.pk)
    }

    /// Derive the recipient address for a given domain
    ///
    /// recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)
    ///
    /// This helper uses the default convention `pk_ivk == pk_spend`.
    pub fn recipient(&self, domain: &Hash32) -> Hash32 {
        recipient_from_pk(domain, &self.pk)
    }

    /// Derive the nullifier key for a given domain
    ///
    /// nf_key = H("NFKEY_V1" || domain || spend_sk)
    ///
    /// Returns None if this PrivacyKey was created from an address (no spend_sk)
    pub fn nf_key(&self, domain: &Hash32) -> Option<Hash32> {
        self.spend_sk().map(|sk| nf_key_from_sk(domain, sk))
    }

    /// Get all derived values for a given domain
    ///
    /// Returns (privacy_address, recipient, nf_key) where:
    /// - privacy_address: bech32m string for user-facing display
    /// - recipient: 32-byte hash used in note commitments
    /// - nf_key: Optional 32-byte hash used in nullifier derivation (None if no spend_sk)
    #[allow(dead_code)]
    pub fn derive_all(&self, domain: &Hash32) -> (String, Hash32, Option<Hash32>) {
        let privacy_address = self.privacy_address().to_string();
        let recipient = self.recipient(domain);
        let nf_key = self.nf_key(domain);

        (privacy_address, recipient, nf_key)
    }

    /// Get the spending secret key as a hex string
    ///
    /// Returns None if this PrivacyKey was created from an address (no spend_sk)
    #[allow(dead_code)]
    pub fn spend_sk_hex(&self) -> Option<String> {
        self.spend_sk().map(|sk| hex::encode(sk))
    }

    /// Get the public key as a hex string
    #[allow(dead_code)]
    pub fn pk_hex(&self) -> String {
        hex::encode(&self.pk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test spending key (32 random bytes for testing)
    const TEST_SPEND_SK_HEX: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // Test domain (32 bytes)
    const TEST_DOMAIN_HEX: &str =
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    #[test]
    fn test_from_hex() {
        let result = PrivacyKey::from_hex(TEST_SPEND_SK_HEX);
        assert!(
            result.is_ok(),
            "Failed to create PrivacyKey: {:?}",
            result.err()
        );

        let key = result.unwrap();
        assert_eq!(key.spend_sk_hex().unwrap(), TEST_SPEND_SK_HEX);
        assert!(key.spend_sk().is_some());
    }

    #[test]
    fn test_from_hex_with_0x_prefix() {
        let key_with_prefix = format!("0x{}", TEST_SPEND_SK_HEX);
        let result = PrivacyKey::from_hex(&key_with_prefix);
        assert!(
            result.is_ok(),
            "Failed to create PrivacyKey with 0x prefix: {:?}",
            result.err()
        );

        let key = result.unwrap();
        assert_eq!(key.spend_sk_hex().unwrap(), TEST_SPEND_SK_HEX);
    }

    #[test]
    fn test_invalid_length() {
        // Too short
        let result = PrivacyKey::from_hex("abcd");
        assert!(result.is_err(), "Should fail with short hex string");

        // Too long
        let result = PrivacyKey::from_hex(&"aa".repeat(33));
        assert!(result.is_err(), "Should fail with long hex string");
    }

    #[test]
    fn test_invalid_hex() {
        let result = PrivacyKey::from_hex("invalid_hex_string_not_hex_chars!");
        assert!(result.is_err(), "Should fail with invalid hex characters");
    }

    #[test]
    fn test_pk_derivation() {
        let key = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        let pk = key.pk();

        assert_eq!(pk.len(), 32, "PK should be 32 bytes");

        // PK should be deterministic
        let key2 = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        assert_eq!(key.pk(), key2.pk(), "PK derivation should be deterministic");
    }

    #[test]
    fn test_recipient_derivation() {
        let key = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        let domain = hex::decode(TEST_DOMAIN_HEX).unwrap();
        let mut domain_bytes = [0u8; 32];
        domain_bytes.copy_from_slice(&domain);

        let recipient = key.recipient(&domain_bytes);

        assert_eq!(recipient.len(), 32, "Recipient should be 32 bytes");

        // Recipient should be deterministic for same domain
        let recipient2 = key.recipient(&domain_bytes);
        assert_eq!(
            recipient, recipient2,
            "Recipient derivation should be deterministic"
        );
    }

    #[test]
    fn test_nf_key_derivation() {
        let key = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        let domain = hex::decode(TEST_DOMAIN_HEX).unwrap();
        let mut domain_bytes = [0u8; 32];
        domain_bytes.copy_from_slice(&domain);

        let nf_key = key.nf_key(&domain_bytes);

        assert!(nf_key.is_some(), "NF key should be derivable with spend_sk");
        assert_eq!(nf_key.unwrap().len(), 32, "NF key should be 32 bytes");

        // NF key should be deterministic for same domain
        let nf_key2 = key.nf_key(&domain_bytes);
        assert_eq!(nf_key, nf_key2, "NF key derivation should be deterministic");
    }

    #[test]
    fn test_derive_all() {
        let key = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        let domain = hex::decode(TEST_DOMAIN_HEX).unwrap();
        let mut domain_bytes = [0u8; 32];
        domain_bytes.copy_from_slice(&domain);

        let (privacy_address, recipient, nf_key) = key.derive_all(&domain_bytes);

        // Privacy address should be bech32m format
        assert!(
            privacy_address.starts_with("privpool1"),
            "Privacy address should start with privpool1"
        );

        // Recipient should match individual derivation
        assert_eq!(recipient, key.recipient(&domain_bytes));

        // NF key should match individual derivation
        assert_eq!(nf_key, key.nf_key(&domain_bytes));
    }

    #[test]
    fn test_privacy_address() {
        let key = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        let addr = key.privacy_address();

        // Should be able to round-trip through string
        let addr_str = addr.to_string();
        assert!(addr_str.starts_with("privpool1"));

        let parsed: PrivacyAddress = addr_str.parse().unwrap();
        assert_eq!(parsed, addr);
    }

    #[test]
    fn test_from_address_public_key_only() {
        // First create a key from spend_sk to get a valid address
        let key_with_sk = PrivacyKey::from_hex(TEST_SPEND_SK_HEX).unwrap();
        let addr_str = key_with_sk.privacy_address().to_string();

        // Now create a key from just the address
        let key_from_addr = PrivacyKey::from_address(&addr_str).unwrap();

        // Public key should match
        assert_eq!(key_from_addr.pk(), key_with_sk.pk());

        // But spend_sk should be None
        assert!(
            key_from_addr.spend_sk().is_none(),
            "Should not have spend_sk when created from address"
        );

        // Recipient derivation should still work
        let domain = [0u8; 32];
        assert_eq!(
            key_from_addr.recipient(&domain),
            key_with_sk.recipient(&domain)
        );

        // But nf_key derivation should return None
        assert!(
            key_from_addr.nf_key(&domain).is_none(),
            "Should not be able to derive nf_key without spend_sk"
        );
    }
}
