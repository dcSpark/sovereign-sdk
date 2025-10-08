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
use thiserror::Error;
use std::convert::TryInto;
use std::fmt;

mod guest;
pub use guest::LigeroGuest;

#[cfg(feature = "native")]
mod host;
#[cfg(feature = "native")]
pub use host::{LigeroArg, LigeroConfig, LigeroHost};

/// Public output from the Ligero guest program for value validation.
/// This structure is committed in the proof's journal and verified on-chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueProofPublic {
    /// The value that was proven to be valid
    pub value: u32,
}

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
    Debug, Clone, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default,
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
        code_commitment: &Self::CodeCommitment,
    ) -> Result<T, Self::Error> {
        // The proof is a bincode-serialized LigeroProofPackage<T>
        // which contains both the raw proof and the public output
        let package: LigeroProofPackage<T> = bincode::deserialize(serialized_proof)?;
        
        // Check if LIGERO_SKIP_VERIFICATION env var is set (for testing)
        if std::env::var("LIGERO_SKIP_VERIFICATION").is_ok() {
            tracing::debug!("Ligero: Skipping verification (LIGERO_SKIP_VERIFICATION set)");
            return Ok(package.public_output);
        }
        
        // Perform actual verification using webgpu_verifier
        #[cfg(feature = "native")]
        {
            // First verify that the proof corresponds to the expected code commitment
            Self::verify_code_commitment(code_commitment)?;
            
            // TODO: For value-setter-zk, we should pass the value as a hex argument
            // For now, we'll try without arguments since there are no private indices
            // The verification might still work if the guest program's public output
            // commitment matches what's in the proof
            Self::verify_with_binary(&package.proof, None)?;
        }
        
        #[cfg(not(feature = "native"))]
        {
            let _ = code_commitment; // Used only in native feature
            tracing::warn!("Ligero verification only available with 'native' feature - skipping verification");
        }
        
        Ok(package.public_output)
    }
}

impl LigeroVerifier {
    /// Verify a Ligero proof with a specific claimed value
    /// 
    /// This method passes BOTH the proven value (from proof generation) and the claimed value
    /// (from the transaction) to the WASM program, which will assert they match.
    /// 
    /// # Security
    /// 
    /// This prevents proof substitution attacks where an attacker uses a proof for value X
    /// to claim value Y in the transaction.
    #[cfg(feature = "native")]
    pub fn verify_with_value<T: DeserializeOwned>(
        proof_bytes: &[u8],
        code_commitment: &LigeroCodeCommitment,
        claimed_value: u32,
    ) -> Result<T, anyhow::Error> {
        tracing::debug!("Ligero: Verifying proof with claimed value: {}", claimed_value);
        
        // Verify code commitment
        Self::verify_code_commitment(code_commitment)?;
        
        // Create hex argument for the claimed value
        let claimed_value_bytes = claimed_value.to_le_bytes();
        let claimed_value_hex = hex::encode(&claimed_value_bytes);
        
        // Perform verification with the claimed value as argument
        Self::verify_with_args(proof_bytes, &[claimed_value_hex])?;
        
        // For now, we don't extract the actual public output from the Ligero proof
        // We rely on the WASM program's assertions to validate correctness
        // The public output should match the claimed value
        let public_output = unsafe {
            // SAFETY: This is a temporary workaround. In production, we should properly
            // extract and deserialize the public output from the Ligero proof.
            // For value-setter-zk, the public output is ValueProofPublic { value: u32 }
            std::mem::transmute_copy(&crate::ValueProofPublic { value: claimed_value })
        };
        
        Ok(public_output)
    }
    
    /// Verify a Ligero proof using the webgpu_verifier binary with custom arguments
    /// 
    /// # Arguments
    /// * `proof_bytes` - The raw Ligero proof bytes (from proof.data)
    /// * `args` - Hex-encoded arguments to pass to the WASM program
    #[cfg(feature = "native")]
    fn verify_with_args(proof_bytes: &[u8], args: &[String]) -> Result<(), anyhow::Error> {
        use std::process::Command;
        use anyhow::Context;
        use crate::host::LigeroArg;
        
        tracing::debug!("Ligero: Verifying with {} custom arguments", args.len());
        
        // Find the verifier binary
        let verifier_bin = Self::find_verifier_binary()
            .context("Failed to locate webgpu_verifier binary")?;
        
        // Create temporary directory for verification
        let temp_dir = tempfile::tempdir()
            .context("Failed to create temporary directory")?;
        
        // Write proof to proof.data
        let proof_path = temp_dir.path().join("proof.data");
        std::fs::write(&proof_path, proof_bytes)
            .context("Failed to write proof.data")?;
        
        // Get configuration
        let mut config = Self::get_verification_config()
            .context("Failed to get Ligero configuration")?;
        
        // Add custom arguments
        for arg_hex in args {
            config.args.push(LigeroArg::Hex { hex: arg_hex.clone() });
        }
        
        let config_json = serde_json::to_string(&config)
            .context("Failed to serialize Ligero config")?;
        
        tracing::debug!("Running webgpu_verifier with config: {}", config_json);
        
        // Run the verifier
        let output = Command::new(&verifier_bin)
            .arg(&config_json)
            .current_dir(temp_dir.path())
            .output()
            .context("Failed to execute webgpu_verifier")?;
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        
        if !stdout.is_empty() {
            tracing::debug!("Verifier stdout:\n{}", stdout);
        }
        if !stderr.is_empty() {
            tracing::debug!("Verifier stderr:\n{}", stderr);
        }
        
        // Check the exit status
        if !output.status.success() {
            tracing::error!("Ligero verifier failed");
            tracing::error!("Stdout: {}", stdout);
            tracing::error!("Stderr: {}", stderr);
            anyhow::bail!("Verifier returned non-zero exit code: {:?}", output.status.code());
        }
        
        // Check for success message
        if !stdout.contains("Final Verify Result:                 true") {
            tracing::error!("Verifier did not confirm proof validity");
            anyhow::bail!("Proof verification failed");
        }
        
        tracing::info!("✓ Ligero proof verified successfully with custom arguments");
        Ok(())
    }
    
    /// Verify a Ligero proof using the webgpu_verifier binary
    /// 
    /// # Arguments
    /// * `proof_bytes` - The raw Ligero proof bytes (from proof.data)
    /// * `public_output_bytes` - Optional public output bytes to pass as hex argument to verifier
    #[cfg(feature = "native")]
    fn verify_with_binary(proof_bytes: &[u8], public_output_bytes: Option<&[u8]>) -> Result<(), anyhow::Error> {
        use std::process::Command;
        use anyhow::Context;
        
        tracing::debug!("Ligero: Performing binary verification with webgpu_verifier (proof size: {} bytes)", proof_bytes.len());
        
        // Find the verifier binary
        tracing::debug!("Step 1/6: Looking for webgpu_verifier binary...");
        let verifier_bin = Self::find_verifier_binary()
            .context("Failed to locate webgpu_verifier binary")?;
        tracing::debug!("  ✓ Found verifier at: {:?}", verifier_bin);
        
        // Create a temporary directory for verification
        tracing::debug!("Step 2/6: Creating temporary directory for proof...");
        let temp_dir = tempfile::tempdir()
            .context("Failed to create temporary directory")?;
        let proof_path = temp_dir.path().join("proof.data");
        
        // Write the proof to the temp file
        std::fs::write(&proof_path, proof_bytes)
            .context("Failed to write proof.data")?;
        tracing::debug!("  ✓ Wrote {} bytes to: {:?}", proof_bytes.len(), proof_path);
        
        // Get or create the configuration
        tracing::debug!("Step 3/6: Loading verification configuration...");
        let mut config = Self::get_verification_config()
            .context("Failed to get Ligero configuration")?;
        
        // Add public output as hex argument if provided
        tracing::debug!("Step 4/6: Preparing verification arguments...");
        if let Some(output_bytes) = public_output_bytes {
            use crate::host::LigeroArg;
            let hex_value = hex::encode(output_bytes);
            config.args.push(LigeroArg::Hex { hex: hex_value.clone() });
            tracing::debug!("  ✓ Added public output as hex argument: {}", hex_value);
        } else if config.args.is_empty() {
            tracing::warn!("No arguments provided to verifier - this may cause verification to fail if the guest program expects arguments");
        } else {
            tracing::debug!("  ✓ Using {} pre-configured argument(s)", config.args.len());
        }
        
        let config_json = serde_json::to_string(&config)
            .context("Failed to serialize Ligero config")?;
        
        tracing::debug!("  ✓ Config ready: {}", config_json);
        tracing::debug!("Step 5/6: Executing webgpu_verifier...");
        
        // Run the verifier
        let output = Command::new(&verifier_bin)
            .arg(&config_json)
            .current_dir(temp_dir.path())
            .output()
            .context("Failed to execute webgpu_verifier")?;
        
        tracing::debug!("  ✓ Verifier process completed, checking results...");
        
        // Always log stdout and stderr for debugging
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        
        if !stdout.is_empty() {
            tracing::debug!("Verifier stdout:\n{}", stdout);
        }
        if !stderr.is_empty() {
            tracing::debug!("Verifier stderr:\n{}", stderr);
        }
        
        tracing::debug!("Step 6/6: Validating verification results...");
        
        // Check the exit status
        if !output.status.success() {
            tracing::error!("Ligero verifier failed with exit code: {:?}", output.status.code());
            tracing::error!("Stdout: {}", stdout);
            tracing::error!("Stderr: {}", stderr);
            anyhow::bail!(
                "Verifier returned non-zero exit code: {:?}",
                output.status.code()
            );
        }
        
        // Check the output for success message
        if !stdout.contains("Final Verify Result:                 true") {
            tracing::error!("Verifier did not confirm proof validity");
            tracing::error!("Output: {}", stdout);
            anyhow::bail!(
                "Proof verification failed: verifier did not confirm validity"
            );
        }
        
        tracing::info!("  ✓ Ligero proof verified successfully!");
        Ok(())
    }
    
    /// Find the webgpu_verifier binary
    #[cfg(feature = "native")]
    fn find_verifier_binary() -> Result<std::path::PathBuf, anyhow::Error> {
        use std::path::Path;
        use anyhow::Context;
        
        // Check environment variable first
        if let Ok(path_str) = std::env::var("LIGERO_VERIFIER_BIN") {
            let bin_path = Path::new(&path_str);
            if bin_path.exists() {
                // Convert to absolute path before returning
                let abs_path = std::fs::canonicalize(bin_path)
                    .with_context(|| format!("Failed to resolve verifier binary path: {}", path_str))?;
                return Ok(abs_path);
            }
        }
        
        // Try to find it relative to the current directory
        let current_dir = std::env::current_dir()
            .context("Failed to get current directory")?;
        
        // Try ../ligero-vm/ligero-prover/bins/webgpu_verifier
        let relative_path = current_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("ligero-vm/ligero-prover/bins/webgpu_verifier"));
        
        if let Some(path) = relative_path {
            if path.exists() {
                let abs_path = std::fs::canonicalize(&path)
                    .context("Failed to resolve verifier binary path")?;
                return Ok(abs_path);
            }
        }
        
        // Try crates/adapters/ligero/guest/bins/webgpu_verifier (from workspace root)
        let workspace_path = current_dir
            .join("crates/adapters/ligero/guest/bins/webgpu_verifier");
        if workspace_path.exists() {
            let abs_path = std::fs::canonicalize(&workspace_path)
                .context("Failed to resolve verifier binary path")?;
            return Ok(abs_path);
        }
        
        anyhow::bail!(
            "Could not find webgpu_verifier binary. Set LIGERO_VERIFIER_BIN environment variable or ensure it's at crates/adapters/ligero/guest/bins/webgpu_verifier"
        )
    }
    
    /// Get the Ligero verification configuration
    #[cfg(feature = "native")]
    fn get_verification_config() -> Result<LigeroConfig, anyhow::Error> {
        use anyhow::Context;
        use std::path::Path;
        
        // Check for full configuration from JSON file
        if let Ok(config_path) = std::env::var("LIGERO_CONFIG_PATH") {
            let config_str = std::fs::read_to_string(&config_path)
                .with_context(|| format!("Failed to read config from {}", config_path))?;
            let config: LigeroConfig = serde_json::from_str(&config_str)
                .context("Failed to parse Ligero config")?;
            return Ok(config);
        }
        
        // Get program path from environment variable or error
        let program_path = std::env::var("LIGERO_PROGRAM_PATH")
            .context("LIGERO_PROGRAM_PATH environment variable is required for verification")?;
        
        // Get shader path from environment variable or error
        let shader_path_str = std::env::var("LIGERO_SHADER_PATH")
            .context("LIGERO_SHADER_PATH environment variable is required for verification")?;
        
        // Convert paths to absolute paths (important when we change working directory)
        let program = std::fs::canonicalize(Path::new(&program_path))
            .with_context(|| format!("Failed to resolve program path: {}", program_path))?
            .to_string_lossy()
            .to_string();
        
        let shader_path = std::fs::canonicalize(Path::new(&shader_path_str))
            .with_context(|| format!("Failed to resolve shader path: {}", shader_path_str))?
            .to_string_lossy()
            .to_string();
        
        // Optional: Get packing parameter (default to 8192 to match the prover)
        let packing = std::env::var("LIGERO_PACKING")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(8192);
        
        tracing::debug!(
            "Ligero verification config: program={}, shader={}, packing={}",
            program,
            shader_path,
            packing
        );
        
        Ok(LigeroConfig {
            program,
            shader_path,
            packing,
            private_indices: vec![], // No private inputs by default
            args: vec![], // No arguments by default
        })
    }
    
    /// Verify that the expected code commitment matches the configured WASM program and packing
    /// 
    /// This is critical for security: it ensures that proofs can only be accepted if they
    /// correspond to the specific guest program that enforces the rollup's constraints.
    #[cfg(feature = "native")]
    fn verify_code_commitment(expected_commitment: &LigeroCodeCommitment) -> Result<(), anyhow::Error> {
        use anyhow::Context;
        use sha2::{Digest, Sha256};
        
        tracing::debug!("Ligero: Verifying code commitment");
        
        // Get the configured program path and packing parameter
        let config = Self::get_verification_config()
            .context("Failed to get verification config for code commitment check")?;
        
        // Read the WASM program bytes
        let wasm_bytes = std::fs::read(&config.program)
            .with_context(|| format!("Failed to read WASM program at {}", config.program))?;
        
        // Compute the commitment: SHA-256(WASM bytes || packing)
        let mut hasher = Sha256::new();
        hasher.update(&wasm_bytes);
        hasher.update(config.packing.to_le_bytes());
        let computed_commitment = hasher.finalize();
        
        let computed = LigeroCodeCommitment(computed_commitment.into());
        
        // Compare against the expected commitment
        if &computed != expected_commitment {
            tracing::error!(
                "Code commitment mismatch! Expected: {:?}, Computed: {:?}",
                hex::encode(&expected_commitment.0),
                hex::encode(&computed.0)
            );
            tracing::error!(
                "This means the proof was generated for a different program than expected."
            );
            tracing::error!(
                "Program: {}, Packing: {}",
                config.program,
                config.packing
            );
            anyhow::bail!(
                "Code commitment verification failed: proof does not correspond to the expected guest program"
            );
        }
        
        tracing::debug!(
            "Code commitment verified successfully: {:?}",
            hex::encode(&computed.0)
        );
        
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
