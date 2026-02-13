//! Integration test for the Nightstream adapter.
//!
//! This test exercises the full prove-verify round-trip:
//! 1. Load the value-validator ROM bytes
//! 2. Configure NightstreamHost with inputs
//! 3. Generate a proof via host.run(true)
//! 4. Verify the proof via NightstreamVerifier::verify
//! 5. Validate the public output matches expectations

#![allow(non_snake_case)]

use sov_nightstream_adapter::{
    NightstreamCodeCommitment, NightstreamHost, NightstreamHostArgs, NightstreamProofPackage,
    NightstreamVerifier,
};
use sov_rollup_interface::zk::{CodeCommitment, ZkVerifier, ZkvmHost};

/// Value-validator ROM bytes (compiled circuit).
mod value_validator_rom {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/circuits/value_validator_rom.rs"
    ));
}

/// Test that the NightstreamCodeCommitment encode/decode works.
#[test]
fn test_code_commitment_roundtrip() {
    let rom_bytes = &value_validator_rom::VALUE_VALIDATOR_ROM;
    let program_base = value_validator_rom::VALUE_VALIDATOR_ROM_BASE;

    let args = NightstreamHostArgs::new(rom_bytes.to_vec(), program_base);
    let host = NightstreamHost::from_args(&args);

    let commitment = host.code_commitment();
    let encoded = commitment.encode();
    let decoded = NightstreamCodeCommitment::decode(&encoded).expect("decode commitment");

    assert_eq!(commitment, decoded);
    assert_eq!(encoded.len(), 32);
}

/// Test the full prove-verify cycle with the value-validator guest program.
///
/// This test:
/// 1. Creates a NightstreamHost with the value-validator ROM
/// 2. Sets input values (proven=42, claimed=42) in RAM
/// 3. Adds an output claim for the expected result
/// 4. Runs the prover
/// 5. Deserializes and verifies the proof
#[test]
fn test_prove_and_verify_value_validator() {
    let rom_bytes = &value_validator_rom::VALUE_VALIDATOR_ROM;
    let program_base = value_validator_rom::VALUE_VALIDATOR_ROM_BASE;

    let test_value: u32 = 42;

    let args = NightstreamHostArgs::new(rom_bytes.to_vec(), program_base);
    let mut host = NightstreamHost::from_args(&args);

    // Configure the host with the value-validator inputs.
    // The guest expects two u32 values at RAM[0x104]:
    //   - proven: u32 (the value to validate)
    //   - claimed: u32 (must equal proven)
    host.add_u32_input(test_value); // proven
    host.add_u32_input(test_value); // claimed

    // Add output claim: the guest writes the validated value to RAM[0x100]
    host.add_output_claim(0x100, test_value as u64);

    // Get the code commitment before proving
    let commitment = host.code_commitment();
    println!("Code commitment: {:?}", commitment);

    // Run the prover
    let proof_bytes = host
        .run(true)
        .expect("proving should succeed");

    println!("Proof package size: {} bytes", proof_bytes.len());

    // Deserialize the proof package
    let package: NightstreamProofPackage =
        bincode::deserialize(&proof_bytes).expect("deserialize proof package");

    println!(
        "Package: public_output size={}, rom size={}",
        package.public_output.len(),
        package.rom_bytes.len()
    );

    // Verify via NightstreamVerifier (using NIGHTSTREAM_SKIP_VERIFICATION for
    // non-native testing, or full verification with native feature)
    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
    struct ValueProofPublic {
        value: u32,
    }

    let result: ValueProofPublic =
        NightstreamVerifier::verify(&proof_bytes, &commitment).expect("verification should succeed");

    assert_eq!(result.value, test_value);
    println!("Verification successful! Proven value: {}", result.value);
}

/// Test that verification fails with a mismatched code commitment.
#[test]
fn test_verify_with_wrong_commitment() {
    let rom_bytes = &value_validator_rom::VALUE_VALIDATOR_ROM;
    let program_base = value_validator_rom::VALUE_VALIDATOR_ROM_BASE;

    let test_value: u32 = 100;

    let args = NightstreamHostArgs::new(rom_bytes.to_vec(), program_base);
    let mut host = NightstreamHost::from_args(&args);

    host.add_u32_input(test_value);
    host.add_u32_input(test_value);
    host.add_output_claim(0x100, test_value as u64);

    let proof_bytes = host.run(true).expect("proving should succeed");

    // Create a wrong commitment
    let wrong_commitment = NightstreamCodeCommitment([0xAA; 32]);

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct ValueProofPublic {
        value: u32,
    }

    // Verification should fail with wrong commitment
    let result: Result<ValueProofPublic, _> =
        NightstreamVerifier::verify(&proof_bytes, &wrong_commitment);

    assert!(
        result.is_err(),
        "Verification should fail with wrong commitment"
    );
}
