//! Authority Viewing Full Key (VFK) management
//!
//! This module provides functionality for managing the authority's Viewing Full Key,
//! which is used to decrypt encrypted notes in the privacy pool.

use anyhow::{Context, Result};

/// Authority VFK context that manages the viewing full key for decrypting notes
///
/// The VFK is a 32-byte key that can be used to decrypt all encrypted notes
/// in the privacy pool (Level-B encryption for authority viewing).
pub struct AuthorityVfk {
    /// The 32-byte viewing full key
    vfk: [u8; 32],
}

impl AuthorityVfk {
    /// Create an AuthorityVfk from a hex string
    ///
    /// # Parameters
    /// * `vfk_hex` - VFK as hex string (with or without "0x" prefix)
    ///
    /// # Example
    /// ```ignore
    /// let authority_vfk = AuthorityVfk::from_hex("fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1")?;
    /// ```
    pub fn from_hex(vfk_hex: impl AsRef<str>) -> Result<Self> {
        let hex_str = vfk_hex.as_ref().trim();
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);

        // Decode hex to bytes
        let vfk_bytes = hex::decode(hex_str).context("Failed to decode VFK hex string")?;

        if vfk_bytes.len() != 32 {
            anyhow::bail!(
                "VFK must be exactly 32 bytes (64 hex characters), got {} bytes",
                vfk_bytes.len()
            );
        }

        let mut vfk = [0u8; 32];
        vfk.copy_from_slice(&vfk_bytes);

        tracing::info!("Authority VFK initialized");
        tracing::debug!("VFK: 0x{}", hex::encode(&vfk));

        Ok(Self { vfk })
    }

    /// Get the raw VFK bytes
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.vfk
    }

    /// Get the VFK as a hex string
    #[allow(dead_code)]
    pub fn to_hex(&self) -> String {
        hex::encode(&self.vfk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test VFK from the decrypt_authority_notes.rs example
    const TEST_VFK_HEX: &str = "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1";

    #[test]
    fn test_from_hex() {
        let result = AuthorityVfk::from_hex(TEST_VFK_HEX);
        assert!(
            result.is_ok(),
            "Failed to create AuthorityVfk: {:?}",
            result.err()
        );

        let vfk = result.unwrap();
        assert_eq!(vfk.to_hex(), TEST_VFK_HEX);
    }

    #[test]
    fn test_from_hex_with_0x_prefix() {
        let vfk_with_prefix = format!("0x{}", TEST_VFK_HEX);
        let result = AuthorityVfk::from_hex(&vfk_with_prefix);
        assert!(
            result.is_ok(),
            "Failed to create AuthorityVfk with 0x prefix: {:?}",
            result.err()
        );

        let vfk = result.unwrap();
        assert_eq!(vfk.to_hex(), TEST_VFK_HEX);
    }

    #[test]
    fn test_invalid_length() {
        // Too short
        let result = AuthorityVfk::from_hex("abcd");
        assert!(result.is_err(), "Should fail with short hex string");

        // Too long
        let result = AuthorityVfk::from_hex(&"aa".repeat(33));
        assert!(result.is_err(), "Should fail with long hex string");
    }

    #[test]
    fn test_invalid_hex() {
        let result = AuthorityVfk::from_hex("invalid_hex_string_not_hex_chars!");
        assert!(result.is_err(), "Should fail with invalid hex characters");
    }

    #[test]
    fn test_as_bytes() {
        let vfk = AuthorityVfk::from_hex(TEST_VFK_HEX).unwrap();
        let bytes = vfk.as_bytes();

        assert_eq!(bytes.len(), 32, "VFK bytes should be 32 bytes");
        assert_eq!(hex::encode(bytes), TEST_VFK_HEX);
    }
}
