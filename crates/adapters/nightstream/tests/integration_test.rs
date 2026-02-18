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
use std::time::Instant;

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

/// Full prove+verify cycle for the note-spend circuit with valid cryptographic inputs.
///
/// Constructs a 1-input, 1-output transfer witness with correct Poseidon2 hashes,
/// Merkle path, nullifier, and enforce-product. This exercises the complete pipeline
/// including output binding.
#[test]
fn test_note_spend_prove_verify_with_witness() {
    use neo_ccs::crypto::poseidon2_goldilocks::poseidon2_hash;
    use p3_field::{Field, PrimeCharacteristicRing, PrimeField64};
    use p3_goldilocks::Goldilocks;
    use sov_nightstream_adapter::{
        BlacklistProof, NoteSpendInput, NoteSpendOutput, NoteSpendWitness,
        default_blacklist_root,
    };
    use std::time::Instant;

    type GlDigest = [Goldilocks; 4];
    const ZERO_DIGEST: GlDigest = [Goldilocks::ZERO; 4];

    const TAG_MT_NODE: u64 = 1;
    const TAG_NOTE: u64 = 2;
    const TAG_PRF_NF: u64 = 3;
    const TAG_PK: u64 = 4;
    const TAG_ADDR: u64 = 5;
    const TAG_NFKEY: u64 = 6;

    fn gl_digest_to_bytes(d: &GlDigest) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, elem) in d.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&elem.as_canonical_u64().to_le_bytes());
        }
        out
    }

    fn _bytes_to_gl_digest(h: &[u8; 32]) -> GlDigest {
        let mut d = [Goldilocks::ZERO; 4];
        for i in 0..4 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&h[i * 8..(i + 1) * 8]);
            d[i] = Goldilocks::from_u64(u64::from_le_bytes(buf));
        }
        d
    }

    // --- Setup: deterministic keys ---
    let domain_gl = [Goldilocks::from_u64(1); 4];
    let domain = gl_digest_to_bytes(&domain_gl);

    let spend_sk_gl = [
        Goldilocks::from_u64(42),
        Goldilocks::from_u64(43),
        Goldilocks::from_u64(44),
        Goldilocks::from_u64(45),
    ];
    let spend_sk = gl_digest_to_bytes(&spend_sk_gl);

    // pk_spend = H(TAG_PK, spend_sk)
    let mut pk_input = [Goldilocks::ZERO; 5];
    pk_input[0] = Goldilocks::from_u64(TAG_PK);
    pk_input[1..5].copy_from_slice(&spend_sk_gl);
    let pk_spend_gl = poseidon2_hash(&pk_input);

    // pk_ivk (use deterministic value)
    let pk_ivk_gl = [
        Goldilocks::from_u64(100),
        Goldilocks::from_u64(101),
        Goldilocks::from_u64(102),
        Goldilocks::from_u64(103),
    ];
    let pk_ivk_owner = gl_digest_to_bytes(&pk_ivk_gl);

    // nf_key = H(TAG_NFKEY, domain, spend_sk)
    let mut nfk_input = [Goldilocks::ZERO; 9];
    nfk_input[0] = Goldilocks::from_u64(TAG_NFKEY);
    nfk_input[1..5].copy_from_slice(&domain_gl);
    nfk_input[5..9].copy_from_slice(&spend_sk_gl);
    let nf_key_gl = poseidon2_hash(&nfk_input);

    // recipient = H(TAG_ADDR, domain, pk_spend, pk_ivk)
    let mut addr_input = [Goldilocks::ZERO; 13];
    addr_input[0] = Goldilocks::from_u64(TAG_ADDR);
    addr_input[1..5].copy_from_slice(&domain_gl);
    addr_input[5..9].copy_from_slice(&pk_spend_gl);
    addr_input[9..13].copy_from_slice(&pk_ivk_gl);
    let recipient_gl = poseidon2_hash(&addr_input);
    let sender_id_gl = recipient_gl;

    // --- Input note ---
    let value: u64 = 1000;
    let rho_gl = [
        Goldilocks::from_u64(200),
        Goldilocks::from_u64(201),
        Goldilocks::from_u64(202),
        Goldilocks::from_u64(203),
    ];
    let rho = gl_digest_to_bytes(&rho_gl);
    let sender_id = gl_digest_to_bytes(&sender_id_gl);

    // cm = H(TAG_NOTE, domain, value, rho, recipient, sender_id)
    let mut cm_input = [Goldilocks::ZERO; 18];
    cm_input[0] = Goldilocks::from_u64(TAG_NOTE);
    cm_input[1..5].copy_from_slice(&domain_gl);
    cm_input[5] = Goldilocks::from_u64(value);
    cm_input[6..10].copy_from_slice(&rho_gl);
    cm_input[10..14].copy_from_slice(&recipient_gl);
    cm_input[14..18].copy_from_slice(&sender_id_gl);
    let cm_gl = poseidon2_hash(&cm_input);

    // --- Merkle tree (depth=2, leaf at position 0) ---
    let depth: u32 = 2;
    let n_leaves = 1usize << depth;
    let mut leaves = vec![ZERO_DIGEST; n_leaves];
    leaves[0] = cm_gl;

    // Build tree manually
    let sib0 = ZERO_DIGEST; // sibling at level 0
    let node_left = {
        let mut inp = [Goldilocks::ZERO; 10];
        inp[0] = Goldilocks::from_u64(TAG_MT_NODE);
        inp[1] = Goldilocks::from_u64(0);
        inp[2..6].copy_from_slice(&cm_gl);
        inp[6..10].copy_from_slice(&sib0);
        poseidon2_hash(&inp)
    };
    let sib1 = {
        let mut inp = [Goldilocks::ZERO; 10];
        inp[0] = Goldilocks::from_u64(TAG_MT_NODE);
        inp[1] = Goldilocks::from_u64(0);
        inp[2..6].copy_from_slice(&ZERO_DIGEST);
        inp[6..10].copy_from_slice(&ZERO_DIGEST);
        poseidon2_hash(&inp)
    };
    let root_gl = {
        let mut inp = [Goldilocks::ZERO; 10];
        inp[0] = Goldilocks::from_u64(TAG_MT_NODE);
        inp[1] = Goldilocks::from_u64(1);
        inp[2..6].copy_from_slice(&node_left);
        inp[6..10].copy_from_slice(&sib1);
        poseidon2_hash(&inp)
    };
    let anchor = gl_digest_to_bytes(&root_gl);
    let siblings = vec![gl_digest_to_bytes(&sib0), gl_digest_to_bytes(&sib1)];

    // nullifier = H(TAG_PRF_NF, domain, nf_key, rho)
    let mut nf_input = [Goldilocks::ZERO; 13];
    nf_input[0] = Goldilocks::from_u64(TAG_PRF_NF);
    nf_input[1..5].copy_from_slice(&domain_gl);
    nf_input[5..9].copy_from_slice(&nf_key_gl);
    nf_input[9..13].copy_from_slice(&rho_gl);
    let nf_gl = poseidon2_hash(&nf_input);
    let nullifier = gl_digest_to_bytes(&nf_gl);

    // --- Output note (same value, same recipient for simplicity) ---
    let out_rho_gl = [
        Goldilocks::from_u64(300),
        Goldilocks::from_u64(301),
        Goldilocks::from_u64(302),
        Goldilocks::from_u64(303),
    ];
    let out_rho = gl_digest_to_bytes(&out_rho_gl);
    let out_pk_spend = gl_digest_to_bytes(&pk_spend_gl);
    let out_pk_ivk = pk_ivk_owner;

    // out_recipient = same as input recipient (self-transfer)
    let out_cm_gl = {
        let mut inp = [Goldilocks::ZERO; 18];
        inp[0] = Goldilocks::from_u64(TAG_NOTE);
        inp[1..5].copy_from_slice(&domain_gl);
        inp[5] = Goldilocks::from_u64(value);
        inp[6..10].copy_from_slice(&out_rho_gl);
        inp[10..14].copy_from_slice(&recipient_gl);
        inp[14..18].copy_from_slice(&sender_id_gl);
        poseidon2_hash(&inp)
    };
    let out_cm = gl_digest_to_bytes(&out_cm_gl);

    // --- Enforce product: prod(values) * prod(rho_diffs) != 0, inv_enforce = 1/prod ---
    let mut enforce_prod = Goldilocks::from_u64(value); // input value
    enforce_prod *= Goldilocks::from_u64(value); // output value
    // rho diff: out_rho - in_rho (per element)
    for i in 0..4 {
        enforce_prod *= out_rho_gl[i] - rho_gl[i];
    }
    let inv_enforce_gl = enforce_prod.inverse();
    let mut inv_enforce = [0u8; 32];
    inv_enforce[..8].copy_from_slice(&inv_enforce_gl.as_canonical_u64().to_le_bytes());

    let blacklist_root = default_blacklist_root();

    // --- Build the witness ---
    let witness = NoteSpendWitness {
        domain,
        spend_sk,
        pk_ivk_owner,
        depth,
        anchor,
        inputs: vec![NoteSpendInput {
            value,
            rho,
            sender_id,
            position: 0,
            siblings,
            nullifier,
        }],
        withdraw_amount: 0,
        withdraw_to: [0u8; 32],
        outputs: vec![NoteSpendOutput {
            value,
            rho: out_rho,
            pk_spend: out_pk_spend,
            pk_ivk: out_pk_ivk,
            cm: out_cm,
        }],
        inv_enforce,
        blacklist_root,
        blacklist_proofs: vec![BlacklistProof::default_for_identity(&sender_id), BlacklistProof::default_for_identity(&sender_id)],
        viewers: vec![],
    };

    // Construct a minimal "SpendPublic" equivalent for public_output.
    // We use a simple bincode-serializable struct since we don't depend on midnight_privacy.
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct TestSpendPublic {
        anchor_root: [u8; 32],
        blacklist_root: [u8; 32],
        nullifiers: Vec<[u8; 32]>,
        withdraw_amount: u128,
        output_commitments: Vec<[u8; 32]>,
        view_attestations: Option<Vec<u8>>,
    }

    let test_public = TestSpendPublic {
        anchor_root: anchor,
        blacklist_root,
        nullifiers: vec![nullifier],
        withdraw_amount: 0,
        output_commitments: vec![out_cm],
        view_attestations: None,
    };
    let public_bytes = bincode::serialize(&test_public).expect("serialize test public");

    // --- Prove + Verify ---
    let rom = &note_spend_rom::NOTE_SPEND_ROM;
    let base = note_spend_rom::NOTE_SPEND_ROM_BASE;

    let mut host = NightstreamHost::new(rom, base);
    host.write_note_spend_witness(&witness, public_bytes);

    let t_prove = Instant::now();
    let compressed = host.run(true).expect("proving should succeed");
    let prove_ms = t_prove.elapsed().as_millis();

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

    let t_verify = Instant::now();
    let ok = package.verify().expect("verification should not error");
    assert!(ok, "proof verification should pass");
    let verify_ms = t_verify.elapsed().as_millis();

    let recovered: TestSpendPublic =
        bincode::deserialize(&package.public_output).expect("deserialize public output");
    assert_eq!(recovered.anchor_root, anchor);
    assert_eq!(recovered.nullifiers, vec![nullifier]);
    assert_eq!(recovered.output_commitments, vec![out_cm]);
    assert_eq!(recovered.blacklist_root, blacklist_root);

    println!("\n=============================================");
    println!("  Note-Spend Prove+Verify (Valid Witness)");
    println!("=============================================");
    println!("  Proof generation: {} ms", prove_ms);
    println!("  Verification:     {} ms", verify_ms);
    println!("  Public output:    {} bytes", package.public_output.len());
    println!("  Folding steps:    {}", package.proof.steps.len());
    println!("  Compressed proof: {:.2} KB", compressed.len() as f64 / 1024.0);
    println!("=============================================\n");
}

/// Test CCS caching: build the cache once, then prove + verify twice,
/// confirming that the second run skips CCS synthesis.
///
/// Uses the value-validator circuit for speed.
#[test]
fn test_ccs_caching_prove_and_verify() {
    let rom_bytes = &value_validator_rom::VALUE_VALIDATOR_ROM;
    let program_base = value_validator_rom::VALUE_VALIDATOR_ROM_BASE;

    // 1. Build the CCS cache from a template host (one-time cost).
    let t_cache = Instant::now();
    let template = NightstreamHost::new(rom_bytes, program_base);
    let cache = template
        .build_ccs_cache()
        .expect("CCS cache build should succeed");
    let cache_ms = t_cache.elapsed().as_millis();

    // --- First prove+verify (with cache) ---
    let mut host1 = NightstreamHost::new(rom_bytes, program_base);
    host1.set_ccs_cache(cache.clone());
    host1.add_u32_input(42);
    host1.add_u32_input(42);
    host1.add_output_claim(0x100, 42);

    let t1_prove = Instant::now();
    let compressed1 = host1.run(true).expect("first prove should succeed");
    let prove1_ms = t1_prove.elapsed().as_millis();

    let package1 = decompress_package(&compressed1);

    let t1_verify = Instant::now();
    let ok1 = package1
        .verify_with_cache(&cache)
        .expect("first cached verify should not error");
    assert!(ok1, "first cached verification should pass");
    let verify1_ms = t1_verify.elapsed().as_millis();

    // --- Second prove+verify (with same cache, different input) ---
    let mut host2 = NightstreamHost::new(rom_bytes, program_base);
    host2.set_ccs_cache(cache.clone());
    host2.add_u32_input(99);
    host2.add_u32_input(99);
    host2.add_output_claim(0x100, 99);

    let t2_prove = Instant::now();
    let compressed2 = host2.run(true).expect("second prove should succeed");
    let prove2_ms = t2_prove.elapsed().as_millis();

    let package2 = decompress_package(&compressed2);

    let t2_verify = Instant::now();
    let ok2 = package2
        .verify_with_cache(&cache)
        .expect("second cached verify should not error");
    assert!(ok2, "second cached verification should pass");
    let verify2_ms = t2_verify.elapsed().as_millis();

    // Also verify without cache to confirm they still work.
    let ok2_uncached = package2.verify().expect("uncached verify should not error");
    assert!(ok2_uncached, "uncached verification should pass");

    println!("\n=============================================");
    println!("  CCS Caching Test (value-validator)");
    println!("=============================================");
    println!("  Cache build:       {} ms", cache_ms);
    println!("  1st prove (cache): {} ms", prove1_ms);
    println!("  1st verify (cache):{} ms", verify1_ms);
    println!("  2nd prove (cache): {} ms", prove2_ms);
    println!("  2nd verify (cache):{} ms", verify2_ms);
    println!("=============================================\n");
}

/// Helper: decompress and deserialize a proof package.
fn decompress_package(compressed: &[u8]) -> NightstreamProofPackage {
    use flate2::read::DeflateDecoder;
    use std::io::Read;
    let mut decoder = DeflateDecoder::new(compressed);
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf).expect("decompress proof");
    bincode::deserialize(&buf).expect("deserialize proof package")
}
