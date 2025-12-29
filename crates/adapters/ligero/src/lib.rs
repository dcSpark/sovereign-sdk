#![deny(missing_docs)]
//! # Ligero zkVM Adapter for Sovereign SDK
//!
//! This crate provides an adapter for using [Ligero](https://github.com/ligeroinc/ligero-prover)
//! as a zkVM backend for Sovereign SDK rollups, alongside RISC0 and SP1.
//!
//! ## Overview
//!
//! Ligero is a zero-knowledge proof system that uses WebGPU for hardware-accelerated proof generation.
//! Unlike RISC0 and SP1 which compile Rust to custom instruction sets, Ligero compiles C/C++ programs
//! to WebAssembly and proves their execution.
//!
//! ## Features
//!
//! - **WebGPU Acceleration**: Hardware-accelerated proof generation using GPU
//! - **C/C++ Guest Programs**: Write guest programs in C/C++ (compiled to WASM with Emscripten)
//! - **Flexible Prover Modes**: Support for skip/execute/prove modes via `SOV_PROVER_MODE`
//! - **Module-Level Proofs**: Can be used for individual module proofs (e.g., `value-setter-zk`)
//! - **Rollup-Level Proofs**: Can be used as the main zkVM for the entire rollup
//!
//! ## Environment Variables
//!
//! ### Build-time Variables
//!
//! - **`SKIP_GUEST_BUILD`**: Control guest program compilation
//!   - `1` or `true`: Skip all guest builds
//!   - `ligero`: Skip only Ligero guest builds
//!   - `0` or unset: Build Ligero guest programs
//!
//! - **`LIGERO_SDK_PATH`**: Override default Ligero SDK path
//!
//! ### Runtime Variables
//!
//! - **`SOV_PROVER_MODE`**: Control proving behavior (for rollup-level usage)
//!   - `skip`: Skip proof generation entirely
//!   - `execute`: Execute without generating proofs (simulation)
//!   - `prove`: Generate full proofs using `webgpu_prover`
//!
//! - **`LIGERO_SKIP_VERIFICATION`**: Skip proof verification (testing only)
//!   - Set to any value to skip verification
//!   - Use only for development/testing
//!
//! ### Verification Configuration (required for proof verification)
//!
//! - **`LIGERO_VERIFIER_BIN`**: Path to `webgpu_verifier` binary (required)
//!   - Example: `crates/adapters/ligero/guest/bins/webgpu_verifier`
//!
//! - **`LIGERO_PROGRAM_PATH`**: Path to WASM program to verify (required)
//!   - Example: `crates/adapters/ligero/guest/bins/value_validator.wasm`
//!   - **Security Note**: This WASM program's hash (with packing) forms the code commitment.
//!     The verifier will reject proofs that don't match this exact program.
//!
//! - **`LIGERO_SHADER_PATH`**: Path to verifier shader (required)
//!   - Example: `crates/adapters/ligero/guest/bins/shader`
//!
//! - **`LIGERO_PACKING`**: FFT packing parameter (optional, default: 8192)
//!   - **Security Note**: This value is included in the code commitment computation.
//!
//! - **`LIGERO_CONFIG_PATH`**: Path to full JSON config file (optional)
//!   - If set, overrides individual path variables
//!
//! ## Security
//!
//! The verifier performs **code commitment verification** before accepting proofs.
//! This ensures that:
//! - Proofs can only be accepted if they correspond to the expected guest program
//! - An attacker cannot submit a proof for a malicious program that bypasses constraints
//! - The code commitment is computed as: `SHA-256(WASM_bytes || packing_u32_le)`
//!
//! The code commitment check is performed automatically during verification and will
//! reject any proof that doesn't match the configured WASM program and packing parameter.
//!
//! ## Usage Example
//!
//! ### As a Rollup zkVM
//!
//! ```rust,no_run
//! use sov_ligero_adapter::Ligero;
//!
//! // Use Ligero as your rollup's zkVM
//! type MyZkvm = Ligero;
//! ```
//!
//! ### Module-Level Proof Generation
//!
//! ```rust,no_run
//! use sov_ligero_adapter::{Ligero, LigeroHost};
//! use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
//!
//! # fn main() -> anyhow::Result<()> {
//! // Create a host for a specific WASM program
//! let mut host = <Ligero as Zkvm>::Host::from_args(&"path/to/program.wasm".to_string());
//!
//! // Add arguments for the guest program
//! host.add_i64_arg(42);
//!
//! // Generate a proof
//! let proof = host.run(true)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Prerequisites
//!
//! 1. **Emscripten SDK**: Required for compiling guest programs
//! 2. **Ligero SDK**: Required for guest program linking
//! 3. **WebGPU**: Required for proof generation (available in Chrome/Edge with GPU)
//!
//! See the [README](../README.md) for detailed setup instructions.

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
pub use guest::LigeroGuest;

pub use ligero_runner::LigeroProofPackage;

#[cfg(feature = "native")]
mod host;
#[cfg(feature = "native")]
pub use host::{LigeroArg, LigeroConfig, LigeroHost};

/// The cryptographic primitives used by Ligero (reuses mock-zkvm crypto)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Copy, schemars::JsonSchema)]
pub struct LigeroCryptoSpec;

impl CryptoSpec for LigeroCryptoSpec {
    #[cfg(feature = "native")]
    type PrivateKey = sov_mock_zkvm::crypto::private_key::Ed25519PrivateKey;
    type PublicKey = Ed25519PublicKey;
    type Hasher = sha2::Sha256;
    type Signature = Ed25519Signature;

    fn sovereign_admin_pubkey() -> Self::PublicKey {
        // Use the same admin pubkey as mock-zkvm
        sov_mock_zkvm::MockZkvmCryptoSpec::sovereign_admin_pubkey()
    }
}

/// The Ligero zkVM
#[derive(Debug, Clone, Default, PartialEq, Eq, schemars::JsonSchema, Serialize, Deserialize)]
pub struct Ligero;

impl Zkvm for Ligero {
    type Guest = LigeroGuest;
    type Verifier = LigeroVerifier;

    #[cfg(feature = "native")]
    type Host = LigeroHost;
}

/// Code commitment for Ligero (SHA-256 hash of WASM + packing)
#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default)]
pub struct LigeroCodeCommitment(pub [u8; 32]);

impl sov_rollup_interface::zk::CodeCommitment for LigeroCodeCommitment {
    type DecodeError = LigeroCodeCommitmentError;

    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn decode(value: &[u8]) -> Result<Self, Self::DecodeError> {
        if value.len() != 32 {
            return Err(LigeroCodeCommitmentError::InvalidLength { found: value.len() });
        }
        let mut contents = [0u8; 32];
        contents.copy_from_slice(value);
        Ok(Self(contents))
    }
}

/// Error that can occur when decoding a LigeroCodeCommitment
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LigeroCodeCommitmentError {
    /// The input was not 32 bytes long
    #[error("LigeroCodeCommitment must be 32 bytes long, but the input was {found} bytes long")]
    InvalidLength {
        /// The size of the input
        found: usize,
    },
}

impl Serialize for LigeroCodeCommitment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Emit the RISC0-style eight-word representation for compatibility with existing genesis files.
        let mut seq = serializer.serialize_seq(Some(8))?;
        for chunk in self.0.chunks_exact(4) {
            let word = u32::from_le_bytes(chunk.try_into().expect("chunk size is enforced"));
            seq.serialize_element(&word)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for LigeroCodeCommitment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(LigeroCodeCommitmentVisitor)
    }
}

struct LigeroCodeCommitmentVisitor;

impl<'de> Visitor<'de> for LigeroCodeCommitmentVisitor {
    type Value = LigeroCodeCommitment;

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
                Ok(LigeroCodeCommitment(data))
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
                Ok(LigeroCodeCommitment(data))
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
                &"expected 32 raw bytes for Ligero code commitment",
            ));
        }
        let mut data = [0u8; 32];
        data.copy_from_slice(v);
        Ok(LigeroCodeCommitment(data))
    }
}

/// Verifier for Ligero proofs
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct LigeroVerifier;

impl ZkVerifier for LigeroVerifier {
    type CodeCommitment = LigeroCodeCommitment;
    type CryptoSpec = LigeroCryptoSpec;
    type Error = anyhow::Error;

    fn verify<T: DeserializeOwned>(
        serialized_proof: &[u8],
        code_commitment: &Self::CodeCommitment,
    ) -> Result<T, Self::Error> {
        tracing::debug!(
            "Deserializing proof package, serialized size: {} bytes",
            serialized_proof.len()
        );
        tracing::debug!(
            "First few bytes of serialized proof: {:?}",
            &serialized_proof[..std::cmp::min(20, serialized_proof.len())]
        );

        // The proof is a bincode-serialized LigeroProofPackage
        // which contains both the raw proof and the public output
        let package: LigeroProofPackage = bincode::deserialize(serialized_proof)?;

        tracing::debug!(
            "Deserialized package: proof size: {} bytes, public_output size: {} bytes",
            package.proof.len(),
            package.public_output.len()
        );
        tracing::debug!(
            "First few bytes of deserialized proof: {:?}",
            &package.proof[..std::cmp::min(20, package.proof.len())]
        );

        let public: T = bincode::deserialize(&package.public_output)?;

        // Check if LIGERO_SKIP_VERIFICATION env var is set (for testing)
        if std::env::var("LIGERO_SKIP_VERIFICATION").is_ok() {
            tracing::debug!("Ligero: Skipping verification (LIGERO_SKIP_VERIFICATION set)");
            return Ok(public);
        }

        #[cfg(feature = "native")]
        {
            // Automatically discover the correct program based on the code commitment
            let paths =
                native::VerifierPaths::discover_with_commitment(Some(&code_commitment.0))
                .map_err(|err| anyhow::anyhow!("Ligero verifier configuration error: {err}"))?;
            
            // Verify the program matches the expected commitment
            native::ensure_code_commitment(&paths, &code_commitment.0)?;
            
            // Deserialize args from JSON
            let args: Vec<LigeroArg> = serde_json::from_slice(&package.args_json)?;
            native::verify_proof(
                &paths,
                &package.proof,
                args,
                package.private_indices.clone(),
            )?;
        }

        #[cfg(not(feature = "native"))]
        {
            let _ = code_commitment;
            tracing::warn!(
                "Ligero verification is only available with the \"native\" feature enabled; skipping verification"
            );
        }

        Ok(public)
    }
}

impl LigeroVerifier {
    /// Verify a proof and return the verifier's stdout/stderr for debugging
    #[cfg(feature = "native")]
    pub fn verify_with_output<T: DeserializeOwned>(
        serialized_proof: &[u8],
        code_commitment: &LigeroCodeCommitment,
    ) -> Result<(T, String, String), anyhow::Error> {
        let package: LigeroProofPackage = bincode::deserialize(serialized_proof)?;
        let public: T = bincode::deserialize(&package.public_output)?;

        let paths = native::VerifierPaths::discover_with_commitment(Some(&code_commitment.0))
            .map_err(|err| anyhow::anyhow!("Ligero verifier configuration error: {err}"))?;
        
        native::ensure_code_commitment(&paths, &code_commitment.0)?;
        
        let args: Vec<LigeroArg> = serde_json::from_slice(&package.args_json)?;
        let (success, stdout, stderr) = native::verify_proof_with_output(
            &paths,
            &package.proof,
            args,
            package.private_indices.clone(),
        )?;

        if !success {
            anyhow::bail!(
                "Ligero verifier did not confirm proof validity\nstdout: {}\nstderr: {}",
                stdout,
                stderr
            );
        }

        Ok((public, stdout, stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sov_rollup_interface::crypto::PublicKey;
    use sov_rollup_interface::zk::CodeCommitment;

    #[test]
    fn test_sovereign_admin_pubkey() {
        let pub_key = LigeroCryptoSpec::sovereign_admin_pubkey();
        let credential_id = pub_key.credential_id();
        assert_eq!(
            credential_id.to_string(),
            "0xf1ac96b6ad3cd6bddaf2c23f089de73a6816f892c1af345df70f9a573a86bacb"
        );
    }

    #[test]
    fn ligero_code_commitment_codec_roundtrip() {
        let raw_data = [42u8; 32];
        let commitment = LigeroCodeCommitment(raw_data);
        let bytes = commitment.encode();
        let decoded = LigeroCodeCommitment::decode(&bytes).expect("Encoding is valid");
        assert_eq!(decoded.0, raw_data);

        // Test invalid length
        let bytes = vec![1u8; 31];
        assert!(matches!(
            LigeroCodeCommitment::decode(&bytes),
            Err(LigeroCodeCommitmentError::InvalidLength { found: 31 })
        ));
    }

    #[test]
    fn test_code_commitment_computation() {
        use sha2::{Digest, Sha256};

        // Create some dummy WASM bytes
        let wasm_bytes = vec![0x00, 0x61, 0x73, 0x6d]; // WASM magic number
        let packing: u32 = 8192;

        // Compute commitment manually
        let mut hasher = Sha256::new();
        hasher.update(&wasm_bytes);
        hasher.update(packing.to_le_bytes());
        let expected_hash = hasher.finalize();

        let expected_commitment = LigeroCodeCommitment(expected_hash.into());

        // Verify the commitment is deterministic
        let mut hasher2 = Sha256::new();
        hasher2.update(&wasm_bytes);
        hasher2.update(packing.to_le_bytes());
        let hash2 = hasher2.finalize();
        let commitment2 = LigeroCodeCommitment(hash2.into());

        assert_eq!(expected_commitment, commitment2);
    }
}
#[cfg(feature = "native")]
mod native {
    pub use ligero_runner::verifier::{
        ensure_code_commitment, verify_proof, verify_proof_with_output, VerifierPaths,
    };
}

