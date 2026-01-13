//! Authority Full Viewing Key (FVK) management
//!
//! This module provides functionality for managing the authority's Full Viewing Key,
//! which is used to decrypt encrypted notes in the privacy pool.

use anyhow::{Context, Result};

/// Authority FVK context that manages the viewing full key for decrypting notes
///
/// The FVK is a 32-byte key that can be used to decrypt all encrypted notes
/// in the privacy pool (Level-B encryption for authority viewing).
pub struct AuthorityFvk {
    /// The 32-byte full viewing key
    fvk: [u8; 32],
}

impl AuthorityFvk {
    /// Create an AuthorityFvk from a hex string
    ///
    /// # Parameters
    /// * `fvk_hex` - FVK as hex string (with or without "0x" prefix)
    pub fn from_hex(fvk_hex: impl AsRef<str>) -> Result<Self> {
        let hex_str = fvk_hex.as_ref().trim();
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);

        // Decode hex to bytes
        let fvk_bytes = hex::decode(hex_str).context("Failed to decode FVK hex string")?;

        if fvk_bytes.len() != 32 {
            anyhow::bail!(
                "FVK must be exactly 32 bytes (64 hex characters), got {} bytes",
                fvk_bytes.len()
            );
        }

        let mut fvk = [0u8; 32];
        fvk.copy_from_slice(&fvk_bytes);

        tracing::info!("Authority FVK initialized");
        tracing::debug!("FVK: 0x{}", hex::encode(&fvk));

        Ok(Self { fvk })
    }

    /// Get the raw FVK bytes
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.fvk
    }

    /// Get the FVK as a hex string
    #[allow(dead_code)]
    pub fn to_hex(&self) -> String {
        hex::encode(&self.fvk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test FVK from the decrypt_authority_notes.rs example
    const TEST_FVK_HEX: &str = "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1";

    #[test]
    fn test_from_hex() {
        let result = AuthorityFvk::from_hex(TEST_FVK_HEX);
        assert!(
            result.is_ok(),
            "Failed to create AuthorityFvk: {:?}",
            result.err()
        );

        let fvk = result.unwrap();
        assert_eq!(fvk.to_hex(), TEST_FVK_HEX);
    }

    #[test]
    fn test_from_hex_with_0x_prefix() {
        let fvk_with_prefix = format!("0x{}", TEST_FVK_HEX);
        let result = AuthorityFvk::from_hex(&fvk_with_prefix);
        assert!(
            result.is_ok(),
            "Failed to create AuthorityFvk with 0x prefix: {:?}",
            result.err()
        );

        let fvk = result.unwrap();
        assert_eq!(fvk.to_hex(), TEST_FVK_HEX);
    }

    #[test]
    fn test_invalid_length() {
        // Too short
        let result = AuthorityFvk::from_hex("abcd");
        assert!(result.is_err(), "Should fail with short hex string");

        // Too long
        let result = AuthorityFvk::from_hex(&"aa".repeat(33));
        assert!(result.is_err(), "Should fail with long hex string");
    }

    #[test]
    fn test_invalid_hex() {
        let result = AuthorityFvk::from_hex("invalid_hex_string_not_hex_chars!");
        assert!(result.is_err(), "Should fail with invalid hex characters");
    }

    #[test]
    fn test_as_bytes() {
        let fvk = AuthorityFvk::from_hex(TEST_FVK_HEX).unwrap();
        let bytes = fvk.as_bytes();

        assert_eq!(bytes.len(), 32, "FVK bytes should be 32 bytes");
        assert_eq!(hex::encode(bytes), TEST_FVK_HEX);
    }
}
