#![cfg(feature = "native")]

//! Integration tests for Ligero proof generation and verification
//! 
//! These tests demonstrate how to:
//! - Create a Ligero host with a WASM guest program
//! - Generate REAL zero-knowledge proofs using WebGPU
//! - Verify proofs using the LigeroVerifier with actual verification
//! - Handle proof verification failures
//!
//! # Requirements
//!
//! To run these tests successfully, you need:
//!
//! 1. **WebGPU-capable hardware and browser/runtime**
//! 2. **Ligero prover binary**: `webgpu_prover` (in `crates/adapters/ligero/bins/`)
//! 3. **Ligero verifier binary**: `webgpu_verifier` (in `crates/adapters/ligero/bins/`)
//! 4. **Shader files**: GPU shaders (in `crates/adapters/ligero/bins/shader/`)
//! 5. **Guest WASM program**: `value_validator.wasm` must be built
//!
//! ## Automatic Configuration
//!
//! These tests use `setup_ligero_env()` which automatically:
//! - Discovers paths to Ligero binaries based on project structure
//! - Sets environment variables (`LIGERO_PROGRAM_PATH`, `LIGERO_VERIFIER_BIN`, etc.)
//! - Validates that required files exist
//!
//! **No manual environment setup required!** Just run the tests.
//!
//! ## Manual Override (Optional)
//!
//! You can manually override paths if needed:
//!
//! ```bash
//! export LIGERO_VERIFIER_BIN="path/to/webgpu_verifier"
//! export LIGERO_PROGRAM_PATH="path/to/value_validator.wasm"
//! export LIGERO_SHADER_PATH="path/to/shader"
//! export LIGERO_PACKING=8192  # optional, defaults to 8192
//! ```
//!
//! These tests generate and verify **REAL** proofs - no simulation or skipping!

use anyhow::{Context, Result};
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment, LigeroVerifier};
use sov_rollup_interface::zk::{CodeCommitment, Zkvm, ZkVerifier, ZkvmHost};
use std::path::PathBuf;

/// Configuration for Ligero test environment
#[derive(Debug)]
struct LigeroTestConfig {
    /// Path to the WASM program
    program_path: PathBuf,
    /// Path to the verifier binary
    verifier_bin: PathBuf,
    /// Path to the shader directory
    shader_path: PathBuf,
    /// FFT packing parameter
    packing: u32,
}

impl LigeroTestConfig {
    /// Discover paths automatically based on project structure
    fn discover() -> Result<Self> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .context("Could not find repository root")?;

        let ligero_dir = repo_root.join("crates/adapters/ligero");
        
        let config = Self {
            program_path: ligero_dir.join("guest/bins/programs/value_validator.wasm"),
            verifier_bin: ligero_dir.join("bins/webgpu_verifier"),
            shader_path: ligero_dir.join("bins/shader"),
            packing: 8192,
        };

        Ok(config)
    }

    /// Apply this configuration to the environment
    fn apply(&self) -> Result<()> {
        // Only set environment variables if they're not already set
        // This allows manual overrides
        if std::env::var("LIGERO_PROGRAM_PATH").is_err() {
            std::env::set_var("LIGERO_PROGRAM_PATH", &self.program_path);
            println!("Set LIGERO_PROGRAM_PATH={}", self.program_path.display());
        }

        if std::env::var("LIGERO_VERIFIER_BIN").is_err() {
            std::env::set_var("LIGERO_VERIFIER_BIN", &self.verifier_bin);
            println!("Set LIGERO_VERIFIER_BIN={}", self.verifier_bin.display());
        }

        if std::env::var("LIGERO_SHADER_PATH").is_err() {
            std::env::set_var("LIGERO_SHADER_PATH", &self.shader_path);
            println!("Set LIGERO_SHADER_PATH={}", self.shader_path.display());
        }

        if std::env::var("LIGERO_PACKING").is_err() {
            std::env::set_var("LIGERO_PACKING", self.packing.to_string());
            println!("Set LIGERO_PACKING={}", self.packing);
        }

        Ok(())
    }

    /// Check if all required files exist
    fn validate(&self) -> Result<()> {
        if !self.program_path.exists() {
            anyhow::bail!(
                "WASM program not found at: {}\nRun: cd crates/adapters/ligero/guest && ./build.sh",
                self.program_path.display()
            );
        }

        // Note: verifier_bin and shader_path might not exist in all environments
        // We'll let those fail at runtime if actually needed
        
        Ok(())
    }
}

/// Setup Ligero test environment with automatic path discovery
/// 
/// This function:
/// 1. Discovers paths to Ligero binaries and programs
/// 2. Sets environment variables for verification
/// 3. Validates that required files exist
/// 
/// Call this at the start of each test that uses Ligero.
fn setup_ligero_env() -> Result<String> {
    let config = LigeroTestConfig::discover()
        .context("Failed to discover Ligero configuration")?;
    
    // Validate that the WASM program exists
    config.validate()
        .context("Ligero configuration validation failed")?;
    
    // Apply environment variables
    config.apply()
        .context("Failed to apply Ligero configuration")?;
    
    // Return the program path for convenience
    Ok(config.program_path.to_string_lossy().to_string())
}

#[test]
fn test_ligero_proof_generation_and_verification() -> Result<()> {
    // Setup Ligero environment and get the program path
    let program_path = setup_ligero_env()?;

    // Create a Ligero host with the value_validator WASM program
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);

    // The value we want to prove is valid (must be in range [0, 65535])
    let proven_value: i64 = 42;
    let claimed_value: i64 = 42;

    // Add arguments for the guest program
    // Arg 1: proven value (the value being proven in ZK)
    // Arg 2: claimed value (from transaction, must match proven value)
    host.add_i64_arg(proven_value);
    host.add_i64_arg(claimed_value);

    // Set the public output (empty for this simple validator)
    host.set_public_output(&())
        .context("Failed to set public output")?;

    // Get the code commitment (this is what would be stored in the module state)
    let code_commitment = host.code_commitment();
    println!(
        "Code commitment (method_id): {}",
        hex::encode(code_commitment.encode())
    );

    // Generate a REAL proof using webgpu_prover
    println!("Generating REAL proof with WebGPU...");
    
    let proof_data = host.run(true)
        .context("Failed to generate proof - do you have webgpu_prover and WebGPU available?")?;

    println!(
        "✓ REAL proof generated successfully! Size: {} bytes",
        proof_data.len()
    );

    // Verify the proof using LigeroVerifier with REAL verification
    println!("Verifying proof with REAL verifier...");
    let _result: () = LigeroVerifier::verify(&proof_data, &code_commitment)
        .context("Proof verification failed - do you have the verifier binaries configured?")?;

    println!("✓ REAL proof verified successfully!");

    Ok(())
}

#[test]
fn test_ligero_proof_with_different_values() -> Result<()> {
    // Setup Ligero environment
    let program_path = setup_ligero_env()?;

    // Test with different valid values
    let test_values = vec![0, 1, 100, 1000, 65535];

    for value in test_values {
        println!("\nTesting with value: {}", value);
        
        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
        host.add_i64_arg(value);
        host.add_i64_arg(value); // claimed value must match proven value
        host.set_public_output(&())?;

        let code_commitment = host.code_commitment();
        
        // Generate REAL proof
        let proof_data = host.run(true)?;
        
        // Verify with REAL verification
        let _result: () = LigeroVerifier::verify(&proof_data, &code_commitment)?;
        
        println!("✓ Value {} verified successfully with REAL proof", value);
    }

    Ok(())
}

#[test]
fn test_ligero_proof_code_commitment_mismatch() -> Result<()> {
    // Setup Ligero environment
    let program_path = setup_ligero_env()?;

    // Generate a REAL proof with one code commitment
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
    host.add_i64_arg(42);
    host.add_i64_arg(42);
    host.set_public_output(&())?;

    let _original_commitment = host.code_commitment();
    let proof_data = host.run(true)?; // Generate REAL proof

    // Try to verify with a different (fake) code commitment
    let fake_commitment = LigeroCodeCommitment([0xFF; 32]);

    // This SHOULD fail due to code commitment mismatch
    let result: Result<(), _> = LigeroVerifier::verify(&proof_data, &fake_commitment);

    // Verification should fail with wrong code commitment
    assert!(
        result.is_err(),
        "Verification should fail when code commitment doesn't match!"
    );

    println!("✓ Code commitment mismatch correctly rejected: {:?}", result.unwrap_err());

    Ok(())
}

#[test]
fn test_ligero_code_commitment_encoding() {
    // Test that code commitments can be encoded and decoded correctly
    let original = LigeroCodeCommitment([42u8; 32]);
    
    let encoded = original.encode();
    assert_eq!(encoded.len(), 32, "Encoded commitment should be 32 bytes");

    let decoded = LigeroCodeCommitment::decode(&encoded)
        .expect("Should decode successfully");
    
    assert_eq!(
        original, decoded,
        "Decoded commitment should match original"
    );

    println!("✓ Code commitment encoding/decoding works correctly");
}

#[test]
fn test_ligero_code_commitment_invalid_length() {
    // Test that decoding fails with invalid length
    let too_short = vec![0u8; 16];
    let result = LigeroCodeCommitment::decode(&too_short);
    assert!(
        result.is_err(),
        "Decoding should fail with incorrect length"
    );

    let too_long = vec![0u8; 64];
    let result = LigeroCodeCommitment::decode(&too_long);
    assert!(
        result.is_err(),
        "Decoding should fail with incorrect length"
    );

    println!("✓ Code commitment length validation works correctly");
}

#[test]
fn test_ligero_proof_value_mismatch_detected() -> Result<()> {
    // This test verifies that the guest program correctly enforces
    // that proven_value == claimed_value
    
    // Setup Ligero environment
    let program_path = setup_ligero_env()?;

    println!("Testing that value mismatch is detected by guest program...");
    
    // Try to prove value=42 but claim value=100
    // This should FAIL during proof generation (guest program will assert)
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
    host.add_i64_arg(42);   // proven_value = 42
    host.add_i64_arg(100);  // claimed_value = 100 (MISMATCH!)
    host.set_public_output(&())?;

    // This should fail because the guest program enforces:
    // assert_one(proven_value == claimed_value);
    let result = host.run(true);
    
    assert!(
        result.is_err(),
        "Proof generation should fail when proven_value != claimed_value"
    );
    
    println!("✓ Value mismatch correctly rejected during proof generation: {:?}", result.unwrap_err());

    Ok(())
}

#[test]
fn test_ligero_proof_with_boundary_values() -> Result<()> {
    // Test boundary values: 0 and 65535 (u16 max)
    
    // Setup Ligero environment
    let program_path = setup_ligero_env()?;

    println!("Testing boundary values with REAL proofs...");
    
    // Test minimum value (0)
    println!("\nTesting minimum value: 0");
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
    host.add_i64_arg(0);
    host.add_i64_arg(0);
    host.set_public_output(&())?;

    let code_commitment = host.code_commitment();
    let proof_data = host.run(true)?;
    
    let _result: () = LigeroVerifier::verify(&proof_data, &code_commitment)?;
    println!("✓ Minimum value (0) verified successfully!");

    // Test maximum value (65535)
    println!("\nTesting maximum value: 65535");
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
    host.add_i64_arg(65535);
    host.add_i64_arg(65535);
    host.set_public_output(&())?;

    let code_commitment = host.code_commitment();
    let proof_data = host.run(true)?;
    
    let _result: () = LigeroVerifier::verify(&proof_data, &code_commitment)?;
    println!("✓ Maximum value (65535) verified successfully!");

    Ok(())
}

