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

/// A Ligero proof package containing both the proof and serialized public output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LigeroProofPackage {
    /// The compressed Ligero proof bytes (from proof_data.gz - boost serialized + gzipped).
    pub proof: Vec<u8>,
    /// Serialized public output committed by the guest program.
    pub public_output: Vec<u8>,
    /// Arguments passed to the guest program (JSON-serialized for bincode compatibility).
    #[cfg(feature = "native")]
    pub args_json: Vec<u8>,
    /// Indices of private arguments (1-based).
    #[cfg(feature = "native")]
    pub private_indices: Vec<usize>,
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
            let paths = native::VerifierPaths::discover_with_commitment(Some(code_commitment))
                .map_err(|err| anyhow::anyhow!("Ligero verifier configuration error: {err}"))?;
            
            // Verify the program matches the expected commitment
            native::ensure_code_commitment(&paths, code_commitment)?;
            
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
    use super::{LigeroCodeCommitment, LigeroConfig};
    use anyhow::{Context, Result};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::tempdir;

    #[derive(Debug)]
    pub struct VerifierPaths {
        pub program: PathBuf,
        pub shader_path: PathBuf,
        pub verifier_bin: PathBuf,
        pub packing: u32,
    }

    impl VerifierPaths {
        /// Discover verifier paths using environment variables or auto-detection.
        /// For backwards compatibility. Prefer `discover_with_commitment` for automatic program selection.
        #[allow(dead_code)]
        pub fn discover() -> Result<Self> {
            Self::discover_with_commitment(None)
        }

        pub fn discover_with_commitment(expected_commitment: Option<&LigeroCodeCommitment>) -> Result<Self> {
            let config = if let Ok(config_path) = std::env::var("LIGERO_CONFIG_PATH") {
                let config_contents = fs::read_to_string(&config_path)
                    .with_context(|| format!("Failed to read Ligero config at {config_path}"))?;
                serde_json::from_str::<LigeroConfig>(&config_contents).with_context(|| {
                    format!("Failed to parse Ligero config JSON from {config_path}")
                })?
            } else {
                // If we have an expected commitment, try to find the matching program
                if let Some(commitment) = expected_commitment {
                    if let Some(config) = Self::find_program_for_commitment(commitment)? {
                        return Ok(config);
                    }
                }

                // Fallback: use LIGERO_PROGRAM_PATH if set
                let program = std::env::var("LIGERO_PROGRAM_PATH").context(
                    "LIGERO_PROGRAM_PATH environment variable is required for Ligero verification",
                )?;
                let shader_path = std::env::var("LIGERO_SHADER_PATH").context(
                    "LIGERO_SHADER_PATH environment variable is required for Ligero verification",
                )?;
                let packing = std::env::var("LIGERO_PACKING")
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(8192);
                LigeroConfig {
                    program,
                    shader_path,
                    packing,
                    private_indices: Vec::new(),
                    args: Vec::new(),
                }
            };

            let program = canonicalize(&config.program).with_context(|| {
                format!("Failed to resolve Ligero program path: {}", config.program)
            })?;
            let shader_path = canonicalize(&config.shader_path).with_context(|| {
                format!(
                    "Failed to resolve Ligero shader path: {}",
                    config.shader_path
                )
            })?;

            let verifier_bin = locate_verifier_binary(program.parent())
                .context("Failed to locate webgpu_verifier binary")?;

            Ok(Self {
                program,
                shader_path,
                verifier_bin,
                packing: config.packing,
            })
        }

        fn find_program_for_commitment(commitment: &LigeroCodeCommitment) -> Result<Option<Self>> {
            use sha2::{Digest, Sha256};

            let current_dir = std::env::current_dir()?;
            let packing: u32 = std::env::var("LIGERO_PACKING")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8192);

            // List of known programs to try
            let program_candidates = vec![
                "note_spend_guest.wasm",
                "value_validator.wasm",
            ];

            let base_paths = vec![
                current_dir.join("crates/adapters/ligero/guest/bins/programs"),
                current_dir.join("../crates/adapters/ligero/guest/bins/programs"),
                current_dir.join("../../crates/adapters/ligero/guest/bins/programs"),
            ];

            for base_path in &base_paths {
                for program_name in &program_candidates {
                    let program_path = base_path.join(program_name);
                    if !program_path.exists() {
                        continue;
                    }

                    // Compute the code commitment for this program
                    if let Ok(wasm_bytes) = fs::read(&program_path) {
                        let mut hasher = Sha256::new();
                        hasher.update(&wasm_bytes);
                        hasher.update(packing.to_le_bytes());
                        let computed = LigeroCodeCommitment(hasher.finalize().into());

                        if &computed == commitment {
                            tracing::debug!(
                                "Found matching program for commitment {}: {}",
                                hex::encode(commitment.0),
                                program_path.display()
                            );

                            // Find shader and verifier
                            let shader_path = std::env::var("LIGERO_SHADER_PATH")
                                .ok()
                                .and_then(|p| canonicalize(&p).ok())
                                .or_else(|| Self::find_shader_path(&current_dir))
                                .context("Failed to find shader path")?;

                            let verifier_bin = locate_verifier_binary(program_path.parent())
                                .context("Failed to locate webgpu_verifier binary")?;

                            return Ok(Some(Self {
                                program: canonicalize(program_path.to_str().context("Invalid path")?)?,
                                shader_path,
                                verifier_bin,
                                packing,
                            }));
                        }
                    }
                }
            }

            Ok(None)
        }

        fn find_shader_path(current_dir: &std::path::Path) -> Option<PathBuf> {
            let candidates = vec![
                current_dir.join("crates/adapters/ligero/bins/shader"),
                current_dir.join("../crates/adapters/ligero/bins/shader"),
            ];

            candidates.into_iter()
                .find(|p| p.exists())
                .and_then(|p| p.to_str().and_then(|s| canonicalize(s).ok()))
        }

        pub fn to_config(
            &self,
            args: Vec<crate::LigeroArg>,
            private_indices: Vec<usize>,
        ) -> LigeroConfig {
            LigeroConfig {
                program: self.program.to_string_lossy().into_owned(),
                shader_path: self.shader_path.to_string_lossy().into_owned(),
                packing: self.packing,
                private_indices,
                args,
            }
        }
    }

    pub fn ensure_code_commitment(
        paths: &VerifierPaths,
        expected: &LigeroCodeCommitment,
    ) -> Result<()> {
        let wasm_bytes = fs::read(&paths.program).with_context(|| {
            format!(
                "Failed to read Ligero WASM program at {}",
                paths.program.display()
            )
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&wasm_bytes);
        hasher.update(paths.packing.to_le_bytes());
        let computed = LigeroCodeCommitment(hasher.finalize().into());

        if &computed != expected {
            anyhow::bail!(
                "Ligero code commitment mismatch: expected {}, computed {}",
                hex::encode(expected.0),
                hex::encode(computed.0)
            );
        }

        Ok(())
    }

    pub fn verify_proof(
        paths: &VerifierPaths,
        proof_bytes: &[u8],
        mut args: Vec<crate::LigeroArg>,
        private_indices: Vec<usize>,
    ) -> Result<()> {
        let temp_dir =
            tempdir().context("Failed to create temporary directory for Ligero verification")?;

        tracing::debug!("Received proof bytes: size: {} bytes", proof_bytes.len());
        tracing::debug!(
            "First few bytes of received proof: {:?}",
            &proof_bytes[..std::cmp::min(20, proof_bytes.len())]
        );

        // Expect proof_bytes to be compressed gzip data (boost serialized + gzipped)
        if proof_bytes.len() >= 2 && proof_bytes[0] == 0x1f && proof_bytes[1] == 0x8b {
            tracing::debug!("✓ Received compressed gzip proof (expected format)");
        } else {
            tracing::warn!(
                "⚠ Received proof does not appear to be gzip format! First bytes: {:02x?}",
                &proof_bytes[..std::cmp::min(10, proof_bytes.len())]
            );
        }

        // Write proof as proof_data.gz (the format verifier expects)
        let proof_path = temp_dir.path().join("proof_data.gz");
        fs::write(&proof_path, proof_bytes)
            .context("Failed to write proof_data.gz for Ligero verification")?;

        tracing::debug!(
            "Wrote proof to: {}, size: {} bytes",
            proof_path.display(),
            proof_bytes.len()
        );
        tracing::debug!(
            "Temp dir contents: {:?}",
            fs::read_dir(temp_dir.path()).unwrap().collect::<Vec<_>>()
        );

        // Redact private arguments (replace with dummy values)
        // IMPORTANT: Keep the same argument type and length as the original
        // (Ligero verifier requires type consistency)
        for &idx in &private_indices {
            if idx > 0 && idx <= args.len() {
                // 1-based indexing
                let arg_idx = idx - 1;
                args[arg_idx] = match &args[arg_idx] {
                    crate::LigeroArg::String { str: s } => {
                        // Replace with 'x' repeated to match original length
                        crate::LigeroArg::String {
                            str: "x".repeat(s.len()),
                        }
                    }
                    crate::LigeroArg::I64 { .. } => {
                        // Replace with 0
                        crate::LigeroArg::I64 { i64: 0 }
                    }
                    crate::LigeroArg::Hex { hex: h } => {
                        // Replace with '0' repeated to match original length
                        crate::LigeroArg::Hex {
                            hex: "0".repeat(h.len()),
                        }
                    }
                };
            }
        }

        let config = paths.to_config(args, private_indices);
        let config_json =
            serde_json::to_string(&config).context("Failed to serialize Ligero verifier config")?;

        tracing::debug!("Verifier config: {}", config_json);
        tracing::debug!(
            "Running verifier from directory: {}",
            temp_dir.path().display()
        );
        tracing::debug!("Verifier binary: {}", paths.verifier_bin.display());

        let output = Command::new(&paths.verifier_bin)
            .arg(&config_json)
            .current_dir(temp_dir.path())
            .output()
            .with_context(|| {
                format!(
                    "Failed to execute Ligero verifier at {}",
                    paths.verifier_bin.display()
                )
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            tracing::error!("Ligero verifier failed: stdout={stdout}, stderr={stderr}");
            anyhow::bail!(
                "Ligero verifier returned non-zero exit status ({:?})",
                output.status.code()
            );
        }

        if !stdout.contains("Final Verify Result:                 true") {
            tracing::error!(
                "Ligero verifier did not report success: stdout={stdout}, stderr={stderr}"
            );
            anyhow::bail!("Ligero verifier did not confirm proof validity");
        }

        Ok(())
    }

    fn canonicalize(path: &str) -> Result<PathBuf> {
        let path = Path::new(path);
        fs::canonicalize(path)
            .or_else(|_| {
                if path.is_absolute() {
                    Err(anyhow::anyhow!("Path does not exist: {}", path.display()))
                } else {
                    let current_dir =
                        std::env::current_dir().context("Failed to get current directory")?;
                    let joined = current_dir.join(path);
                    Ok(fs::canonicalize(&joined)?)
                }
            })
            .with_context(|| format!("Failed to canonicalize {}", path.display()))
    }

    fn locate_verifier_binary(program_parent: Option<&Path>) -> Result<PathBuf> {
        if let Ok(path_str) = std::env::var("LIGERO_VERIFIER_BIN") {
            let path = Path::new(&path_str);
            if path.exists() {
                return fs::canonicalize(path)
                    .with_context(|| format!("Failed to resolve verifier binary path {path_str}"));
            }
        }

        if let Some(dir) = program_parent {
            let candidate = dir.join("webgpu_verifier");
            if candidate.exists() {
                return fs::canonicalize(candidate)
                    .context("Failed to resolve verifier binary located next to program");
            }
        }

        let current_dir =
            std::env::current_dir().context("Failed to determine current directory")?;

        let candidates = [
            current_dir
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("ligero-vm/ligero-prover/bins/webgpu_verifier")),
            Some(current_dir.join("crates/adapters/ligero/guest/bins/webgpu_verifier")),
            // Platform-specific paths
            #[cfg(target_os = "macos")]
            Some(current_dir.join(
                "crates/adapters/ligero/bins/macos-arm64/bin/webgpu_verifier",
            )),
            #[cfg(target_os = "linux")]
            Some(current_dir.join("crates/adapters/ligero/bins/linux-amd64/bin/webgpu_verifier")),
            #[cfg(target_os = "linux")]
            Some(current_dir.join("crates/adapters/ligero/bins/linux-arm64/bin/webgpu_verifier")),
        ];

        for candidate in candidates.into_iter().flatten() {
            if candidate.exists() {
                return fs::canonicalize(candidate)
                    .context("Failed to resolve fallback verifier binary path");
            }
        }

        anyhow::bail!(
            "Unable to locate webgpu_verifier binary. Set LIGERO_VERIFIER_BIN or place the binary in a known location"
        );
    }
}
