#![deny(missing_docs)]
//! # Nightstream zkVM Adapter for Sovereign SDK
//!
//! This crate provides an adapter for using [Nightstream](https://github.com/nicbou/nightstream)
//! as a zkVM backend for Sovereign SDK rollups, alongside RISC0, SP1, and Ligero.
//!
//! ## Overview
//!
//! Nightstream is a post-quantum, lattice-based folding proof system. It proves RISC-V
//! execution using CCS (Customizable Constraint System) over Goldilocks fields with Ajtai
//! commitments, Twist (R/W memory), and Shout (read-only lookups).
//!
//! Unlike RISC0 which operates on ELF binaries, Nightstream uses ROM bytes extracted from
//! the `.neo_start` section of a RISC-V ELF binary compiled with the `nightstream-sdk`.
//!
//! ## Features
//!
//! - **Post-Quantum Security**: Lattice-based Ajtai commitments
//! - **RISC-V Guest Programs**: Write guest programs using `nightstream-sdk`'s `#[provable]` macro
//! - **CCS Folding**: Efficient folding-based proof generation via Π-CCS
//!
//! ## Environment Variables
//!
//! - **`NIGHTSTREAM_SKIP_VERIFICATION`**: Skip proof verification (testing only)

use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::{self, DeserializeOwned, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};
use sov_mock_zkvm::crypto::{Ed25519PublicKey, Ed25519Signature};
use sov_rollup_interface::zk::{CryptoSpec, ZkVerifier, Zkvm};
use std::convert::TryInto;
use std::fmt;
use thiserror::Error;

mod guest;
pub use guest::NightstreamGuest;

#[cfg(feature = "native")]
mod host;
#[cfg(feature = "native")]
pub use host::{NightstreamHost, NightstreamHostArgs};

mod proof_package;
pub use proof_package::NightstreamProofPackage;

/// The cryptographic primitives used by Nightstream (reuses mock-zkvm crypto).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Copy, schemars::JsonSchema)]
pub struct NightstreamCryptoSpec;

impl CryptoSpec for NightstreamCryptoSpec {
    #[cfg(feature = "native")]
    type PrivateKey = sov_mock_zkvm::crypto::private_key::Ed25519PrivateKey;
    type PublicKey = Ed25519PublicKey;
    type Hasher = sha2::Sha256;
    type Signature = Ed25519Signature;

    fn sovereign_admin_pubkey() -> Self::PublicKey {
        sov_mock_zkvm::MockZkvmCryptoSpec::sovereign_admin_pubkey()
    }
}

/// The Nightstream zkVM.
#[derive(Debug, Clone, Default, PartialEq, Eq, schemars::JsonSchema, Serialize, Deserialize)]
pub struct Nightstream;

impl Zkvm for Nightstream {
    type Guest = NightstreamGuest;
    type Verifier = NightstreamVerifier;

    #[cfg(feature = "native")]
    type Host = NightstreamHost;
}

/// Code commitment for Nightstream (SHA-256 hash of ROM bytes).
///
/// The ROM bytes are extracted from the `.neo_start` section of a RISC-V ELF binary
/// compiled with the `nightstream-sdk`. This commitment uniquely identifies the guest
/// program being proven, analogous to RISC0's MethodId.
#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default)]
pub struct NightstreamCodeCommitment(pub [u8; 32]);

impl sov_rollup_interface::zk::CodeCommitment for NightstreamCodeCommitment {
    type DecodeError = NightstreamCodeCommitmentError;

    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn decode(value: &[u8]) -> Result<Self, Self::DecodeError> {
        if value.len() != 32 {
            return Err(NightstreamCodeCommitmentError::InvalidLength {
                found: value.len(),
            });
        }
        let mut contents = [0u8; 32];
        contents.copy_from_slice(value);
        Ok(Self(contents))
    }
}

/// Error that can occur when decoding a NightstreamCodeCommitment.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NightstreamCodeCommitmentError {
    /// The input was not 32 bytes long.
    #[error("NightstreamCodeCommitment must be 32 bytes long, but the input was {found} bytes long")]
    InvalidLength {
        /// The size of the input.
        found: usize,
    },
}

impl Serialize for NightstreamCodeCommitment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(8))?;
        for chunk in self.0.chunks_exact(4) {
            let word = u32::from_le_bytes(chunk.try_into().expect("chunk size is enforced"));
            seq.serialize_element(&word)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for NightstreamCodeCommitment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(NightstreamCodeCommitmentVisitor)
    }
}

struct NightstreamCodeCommitmentVisitor;

impl<'de> Visitor<'de> for NightstreamCodeCommitmentVisitor {
    type Value = NightstreamCodeCommitment;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a byte array of length 32 or eight 32-bit words")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut elements = Vec::new();
        while let Some(value) = seq.next_element::<u64>()? {
            elements.push(value);
        }

        match elements.len() {
            32 => {
                let mut data = [0u8; 32];
                for (idx, byte) in elements.iter().enumerate() {
                    if *byte > u8::MAX as u64 {
                        return Err(de::Error::invalid_value(
                            de::Unexpected::Unsigned(*byte),
                            &"a byte-sized value (0-255)",
                        ));
                    }
                    data[idx] = *byte as u8;
                }
                Ok(NightstreamCodeCommitment(data))
            }
            8 => {
                let mut data = [0u8; 32];
                for (idx, word) in elements.iter().enumerate() {
                    if *word > u32::MAX as u64 {
                        return Err(de::Error::invalid_value(
                            de::Unexpected::Unsigned(*word),
                            &"a 32-bit unsigned integer",
                        ));
                    }
                    let word_bytes = (*word as u32).to_le_bytes();
                    let start = idx * 4;
                    data[start..start + 4].copy_from_slice(&word_bytes);
                }
                Ok(NightstreamCodeCommitment(data))
            }
            len => Err(de::Error::invalid_length(
                len,
                &"expected either 32 byte values or eight 32-bit words",
            )),
        }
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if v.len() != 32 {
            return Err(E::invalid_length(
                v.len(),
                &"expected 32 raw bytes for Nightstream code commitment",
            ));
        }
        let mut data = [0u8; 32];
        data.copy_from_slice(v);
        Ok(NightstreamCodeCommitment(data))
    }
}

/// Verifier for Nightstream proofs.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct NightstreamVerifier;

impl ZkVerifier for NightstreamVerifier {
    type CodeCommitment = NightstreamCodeCommitment;
    type CryptoSpec = NightstreamCryptoSpec;
    type Error = anyhow::Error;

    fn verify<T: DeserializeOwned>(
        serialized_proof: &[u8],
        code_commitment: &Self::CodeCommitment,
    ) -> Result<T, Self::Error> {
        tracing::debug!(
            "Deserializing Nightstream proof package, compressed size: {} bytes",
            serialized_proof.len()
        );

        // Decompress (DEFLATE) then deserialize (bincode)
        let decompressed = {
            use flate2::read::DeflateDecoder;
            use std::io::Read;
            let mut decoder = DeflateDecoder::new(serialized_proof);
            let mut buf = Vec::new();
            decoder.read_to_end(&mut buf).map_err(|e| {
                anyhow::anyhow!("Failed to decompress Nightstream proof: {}", e)
            })?;
            buf
        };

        tracing::debug!(
            "Decompressed proof package: {} bytes -> {} bytes",
            serialized_proof.len(),
            decompressed.len()
        );

        let package: NightstreamProofPackage = bincode::deserialize(&decompressed)?;

        tracing::debug!(
            "Deserialized package: public_output size: {} bytes, rom size: {} bytes",
            package.public_output.len(),
            package.rom_bytes.len()
        );

        // Deserialize the public output
        let public: T = bincode::deserialize(&package.public_output)?;

        // Check if NIGHTSTREAM_SKIP_VERIFICATION env var is set (for testing)
        if std::env::var("NIGHTSTREAM_SKIP_VERIFICATION").is_ok() {
            tracing::warn!(
                "Nightstream: Skipping verification (NIGHTSTREAM_SKIP_VERIFICATION set)"
            );
            return Ok(public);
        }

        #[cfg(feature = "native")]
        {
            // Verify that the ROM matches the expected code commitment
            tracing::info!("Nightstream: verifying code commitment...");
            native::ensure_code_commitment(&package.rom_bytes, &code_commitment.0)?;
            tracing::info!("Nightstream: code commitment OK");

            // Verify the proof (verify-only, no re-execution)
            tracing::info!(
                "Nightstream: starting proof verification (verify-only, {} step instances)...",
                package.steps_public.len(),
            );
            native::verify_proof_package(&package)?;
            tracing::info!("Nightstream: proof verification PASSED");
        }

        #[cfg(not(feature = "native"))]
        {
            let _ = code_commitment;
            tracing::warn!(
                "Nightstream verification is only available with the \"native\" feature enabled; \
                 skipping verification"
            );
        }

        Ok(public)
    }
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use sha2::Digest;

    /// Compute the code commitment (SHA-256 hash) for ROM bytes.
    pub fn compute_code_commitment(rom_bytes: &[u8]) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        hasher.update(rom_bytes);
        hasher.finalize().into()
    }

    /// Ensure that the ROM bytes match the expected code commitment.
    pub fn ensure_code_commitment(
        rom_bytes: &[u8],
        expected: &[u8; 32],
    ) -> Result<(), anyhow::Error> {
        let actual = compute_code_commitment(rom_bytes);
        if actual != *expected {
            anyhow::bail!(
                "Nightstream code commitment mismatch: expected {:?}, got {:?}",
                hex::encode(expected),
                hex::encode(actual)
            );
        }
        Ok(())
    }

    /// Verify a Nightstream proof package (verify-only, no re-execution).
    ///
    /// This delegates to `NightstreamProofPackage::verify()` which:
    /// 1. Reconstructs the CCS structure from the ROM + config (circuit synthesis only, ~ms).
    /// 2. Verifies the `ShardProof` against the provided `mcss_public` instances.
    ///
    /// **No RISC-V execution or re-proving is performed.**
    pub fn verify_proof_package(package: &NightstreamProofPackage) -> Result<(), anyhow::Error> {
        let ok = package.verify().map_err(|e| {
            anyhow::anyhow!("Nightstream proof verification failed: {:?}", e)
        })?;

        if !ok {
            anyhow::bail!("Nightstream proof verification returned false");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sov_rollup_interface::crypto::PublicKey;
    use sov_rollup_interface::zk::CodeCommitment;

    #[test]
    fn test_sovereign_admin_pubkey() {
        let pub_key = NightstreamCryptoSpec::sovereign_admin_pubkey();
        let credential_id = pub_key.credential_id();
        assert_eq!(
            credential_id.to_string(),
            "0xf1ac96b6ad3cd6bddaf2c23f089de73a6816f892c1af345df70f9a573a86bacb"
        );
    }

    #[test]
    fn nightstream_code_commitment_codec_roundtrip() {
        let raw_data = [42u8; 32];
        let commitment = NightstreamCodeCommitment(raw_data);
        let bytes = commitment.encode();
        let decoded = NightstreamCodeCommitment::decode(&bytes).expect("Encoding is valid");
        assert_eq!(decoded.0, raw_data);

        // Test invalid length
        let bytes = vec![1u8; 31];
        assert!(matches!(
            NightstreamCodeCommitment::decode(&bytes),
            Err(NightstreamCodeCommitmentError::InvalidLength { found: 31 })
        ));
    }

    #[test]
    fn test_code_commitment_computation() {
        use sha2::Digest;

        let rom_bytes = vec![0x00, 0x61, 0x73, 0x6d];

        let mut hasher = sha2::Sha256::new();
        hasher.update(&rom_bytes);
        let expected_hash = hasher.finalize();

        let expected_commitment = NightstreamCodeCommitment(expected_hash.into());

        let mut hasher2 = sha2::Sha256::new();
        hasher2.update(&rom_bytes);
        let hash2 = hasher2.finalize();
        let commitment2 = NightstreamCodeCommitment(hash2.into());

        assert_eq!(expected_commitment, commitment2);
    }
}
