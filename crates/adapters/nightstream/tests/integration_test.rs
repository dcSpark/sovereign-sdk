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

/// Note-spend (placeholder echo) ROM bytes.
mod note_spend_rom {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/circuits/note_spend_rom.rs"
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

    // Decompress (DEFLATE) then deserialize the proof package
    let decompressed = {
        use flate2::read::DeflateDecoder;
        use std::io::Read;
        let mut decoder = DeflateDecoder::new(proof_bytes.as_slice());
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).expect("decompress proof");
        buf
    };
    let package: NightstreamProofPackage =
        bincode::deserialize(&decompressed).expect("deserialize proof package");

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

/// Benchmark the note_spend echo circuit: proof generation time,
/// verification time, and proof size breakdown.
///
/// Simulates a ~500 byte SpendPublic payload (typical bincode-serialized
/// size for a 1-input / 1-output transfer).
#[test]
fn test_note_spend_echo_bench() {
    use std::time::Instant;

    let rom = &note_spend_rom::NOTE_SPEND_ROM;
    let base = note_spend_rom::NOTE_SPEND_ROM_BASE;

    // Simulate a small SpendPublic payload (20 bytes = 5 u32 words).
    // Use a small payload so the test completes quickly in debug builds.
    // In production, payloads are ~200-500 bytes; proving time scales
    // roughly linearly with the number of output claims.
    let fake_payload: Vec<u8> = (0..20).map(|i| (i % 256) as u8).collect();

    let mut host = NightstreamHost::new(rom, base);
    host.add_public_output_bytes(&fake_payload);

    // --- Proof generation ---
    let t_prove = Instant::now();
    let compressed = host.run(true).expect("proving should succeed");
    let prove_ms = t_prove.elapsed().as_millis();

    // Decompress
    let decompressed = {
        use flate2::read::DeflateDecoder;
        use std::io::Read;
        let mut decoder = DeflateDecoder::new(compressed.as_slice());
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).expect("decompress proof");
        buf
    };

    let package: NightstreamProofPackage =
        bincode::deserialize(&decompressed).expect("deserialize proof package");

    // --- Verification ---
    let t_verify = Instant::now();
    let ok = package.verify().expect("verification should not error");
    assert!(ok, "proof verification should return true");
    let verify_ms = t_verify.elapsed().as_millis();

    // Per-field sizes
    let sz_proof = bincode::serialized_size(&package.proof).unwrap();
    let sz_steps = bincode::serialized_size(&package.steps_public).unwrap();
    let sz_rom = bincode::serialized_size(&package.rom_bytes).unwrap();
    let sz_config = bincode::serialized_size(&package.config).unwrap();
    let sz_output = bincode::serialized_size(&package.public_output).unwrap();
    let total_raw = sz_proof + sz_steps + sz_rom + sz_config + sz_output;

    println!("\n=============================================");
    println!("  Note-Spend Echo Circuit Benchmark");
    println!("=============================================");
    println!("  Proof generation: {} ms", prove_ms);
    println!("  Verification:     {} ms", verify_ms);
    println!("---------------------------------------------");
    println!("  Payload input:    {} bytes", fake_payload.len());
    println!("  Public output:    {} bytes", package.public_output.len());
    println!("  ROM size:         {} bytes", package.rom_bytes.len());
    println!("  Folding steps:    {}", package.proof.steps.len());
    println!("  Step instances:   {}", package.steps_public.len());
    println!("---------------------------------------------");
    println!("  proof:            {:.2} KB", sz_proof as f64 / 1024.0);
    println!("  steps_public:     {:.2} KB", sz_steps as f64 / 1024.0);
    println!("  rom_bytes:        {:.2} KB", sz_rom as f64 / 1024.0);
    println!("  config:           {:.2} KB", sz_config as f64 / 1024.0);
    println!("  public_output:    {:.2} KB", sz_output as f64 / 1024.0);
    println!("---------------------------------------------");
    println!("  Raw total:        {:.2} KB", total_raw as f64 / 1024.0);
    println!("  Decompressed:     {:.2} KB", decompressed.len() as f64 / 1024.0);
    println!("  Compressed:       {:.2} KB", compressed.len() as f64 / 1024.0);
    println!(
        "  Compression:      {:.0}% reduction",
        (1.0 - compressed.len() as f64 / decompressed.len() as f64) * 100.0
    );
    println!("=============================================\n");

    // Sanity: public_output should contain the padded payload
    assert!(
        package.public_output.len() >= fake_payload.len(),
        "public_output should be at least as large as the input payload"
    );
    assert_eq!(
        &package.public_output[..fake_payload.len()],
        &fake_payload[..],
        "public_output should start with the original payload"
    );
}
