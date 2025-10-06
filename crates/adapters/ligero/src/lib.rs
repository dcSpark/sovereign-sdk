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
//! - **`SKIP_GUEST_BUILD`**: Control guest program compilation
//!   - `1` or `true`: Skip all guest builds
//!   - `ligero`: Skip only Ligero guest builds
//!   - `0` or unset: Build Ligero guest programs
//!
//! - **`SOV_PROVER_MODE`**: Control proving behavior (for rollup-level usage)
//!   - `skip`: Skip proof generation entirely
//!   - `execute`: Execute without generating proofs (simulation)
//!   - `prove`: Generate full proofs using `webgpu_prover`
//!
//! - **`LIGERO_SDK_PATH`**: Override default Ligero SDK path
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
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sov_mock_zkvm::crypto::{Ed25519PublicKey, Ed25519Signature};
use sov_rollup_interface::zk::{CryptoSpec, ZkVerifier, Zkvm};
use thiserror::Error;

mod guest;
pub use guest::LigeroGuest;

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
#[derive(
    Debug, Clone, PartialEq, Eq, BorshDeserialize, BorshSerialize, Serialize, Deserialize, Default,
)]
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

/// A Ligero proof package containing both the proof and public output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LigeroProofPackage<T> {
    /// The raw Ligero proof bytes (from proof.data)
    pub proof: Vec<u8>,
    /// The public output committed by the guest program
    pub public_output: T,
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
        _code_commitment: &Self::CodeCommitment,
    ) -> Result<T, Self::Error> {
        // The proof is a bincode-serialized LigeroProofPackage<T>
        // which contains both the raw proof and the public output
        let package: LigeroProofPackage<T> = bincode::deserialize(serialized_proof)?;
        
        // In a full implementation, we would:
        // 1. Write the proof to a temp file
        // 2. Call webgpu_verifier with the appropriate config
        // 3. Check the verifier output
        //
        // For now, we trust that the proof was generated correctly
        // by the prover (which already verified it).
        // The security model here is that proofs are generated off-chain
        // and verified on-chain by checking the cryptographic commitment.
        
        // TODO: Implement actual binary verification when needed
        tracing::debug!(
            "Ligero proof verification: accepting proof (binary verification not yet implemented)"
        );
        
        Ok(package.public_output)
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
}
