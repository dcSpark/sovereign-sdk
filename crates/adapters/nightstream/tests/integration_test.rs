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

/// Note-deposit ROM bytes.
mod note_deposit_rom {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/circuits/note_deposit_rom.rs"
    ));
}

const NOTE_SPEND_TLEN_AUTOTUNE_CAP: usize = usize::MAX;
const NOTE_DEPOSIT_TLEN_AUTOTUNE_CAP: usize = usize::MAX;

fn exec_table_from_sim(
    sim: &neo_vm_trace::VmTrace<u64, u64, u128>,
) -> neo_memory::riscv::exec_table::RiscvExecTable {
    neo_memory::riscv::exec_table::RiscvExecTable::from_trace_padded_with_xlen(
        sim,
        sim.steps.len(),
        64,
    )
    .expect("simulation exec table")
}

fn simulate_rv64_elf(
    elf: &[u8],
    ram_pairs: &[(u64, u32)],
    max_steps: usize,
) -> neo_vm_trace::VmTrace<u64, u64, u128> {
    use neo_fold::rv64_trace_shard::Rv64TraceWiring;

    let mut wiring = Rv64TraceWiring::from_elf(elf)
        .expect("load RV64 guest")
        .max_steps(max_steps);
    for &(addr, val) in ram_pairs {
        wiring = wiring.ram_init_u32(addr, val);
    }
    wiring.simulate().expect("simulation trace")
}

fn max_consecutive_pc_run(exec: &neo_memory::riscv::exec_table::RiscvExecTable) -> usize {
    let mut best = 1usize;
    let mut cur = 0usize;
    let mut prev_pc: Option<u64> = None;
    for row in exec.rows.iter().filter(|r| r.active) {
        if prev_pc == Some(row.pc_before) {
            cur += 1;
        } else {
            cur = 1;
            prev_pc = Some(row.pc_before);
        }
        best = best.max(cur);
    }
    best
}

fn boundary_splits_virtual_sequence(
    exec: &neo_memory::riscv::exec_table::RiscvExecTable,
    chunk_rows: usize,
) -> bool {
    if chunk_rows == 0 {
        return false;
    }
    let total = exec.rows.len();
    if total <= chunk_rows {
        return false;
    }
    let mut boundary = chunk_rows;
    while boundary < total {
        let prev = &exec.rows[boundary - 1];
        if prev.active && prev.is_virtual {
            return true;
        }
        boundary = match boundary.checked_add(chunk_rows) {
            Some(next) => next,
            None => break,
        };
    }
    false
}

fn effective_step_rows_for_requested(
    exec: &neo_memory::riscv::exec_table::RiscvExecTable,
    requested_chunk_rows_arch: usize,
) -> usize {
    let requested_chunk_rows = requested_chunk_rows_arch.max(max_consecutive_pc_run(exec));
    let base_step_rows = requested_chunk_rows.min(exec.rows.len().max(1));
    let mut step_rows = base_step_rows;
    while step_rows < exec.rows.len() && boundary_splits_virtual_sequence(exec, step_rows) {
        step_rows = step_rows.saturating_add(base_step_rows);
        if step_rows == usize::MAX {
            break;
        }
    }
    step_rows.min(exec.rows.len().max(1))
}

fn auto_tune_chunk_rows(
    exec: &neo_memory::riscv::exec_table::RiscvExecTable,
    requested_chunk_rows_arch: usize,
    t_len_cap: usize,
) -> (usize, usize) {
    let search_max = requested_chunk_rows_arch
        .min(exec.rows.len().max(1))
        .min(t_len_cap)
        .max(1);
    let total_rows = exec.rows.len().max(1);
    let mut best = None::<(usize, usize, usize)>;
    for candidate in 1..=search_max {
        let step_rows = effective_step_rows_for_requested(exec, candidate);
        if step_rows > t_len_cap {
            continue;
        }
        let fold_count = total_rows.div_ceil(step_rows.max(1));
        match best {
            Some((best_chunk_rows, best_step_rows, best_fold_count))
                if fold_count > best_fold_count
                    || (fold_count == best_fold_count && step_rows < best_step_rows)
                    || (fold_count == best_fold_count
                        && step_rows == best_step_rows
                        && candidate >= best_chunk_rows) => {}
            _ => best = Some((candidate, step_rows, fold_count)),
        }
    }
    best.map(|(chunk_rows, step_rows, _)| (chunk_rows, step_rows))
        .unwrap_or((1usize, effective_step_rows_for_requested(exec, 1usize)))
}

fn parse_usize_after(msg: &str, key: &str) -> Option<usize> {
    let start = msg.find(key)? + key.len();
    let rest = &msg[start..];
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    rest[..digits_end].parse::<usize>().ok()
}

fn poseidon_split_next_chunk_rows(msg: &str, current_chunk_rows: usize) -> Option<usize> {
    if !msg.contains("poseidon split") {
        return None;
    }
    let ccs_m = parse_usize_after(msg, "ccs_m=")?;
    let m_in = parse_usize_after(msg, "m_in=")?;
    let t_len = parse_usize_after(msg, "t_len=")?;
    let cap = ccs_m.saturating_sub(m_in).max(1);
    if t_len <= cap {
        return None;
    }
    let mut next = current_chunk_rows
        .saturating_mul(cap)
        .checked_div(t_len)
        .unwrap_or(0)
        .max(1);
    if next >= current_chunk_rows {
        next = current_chunk_rows.saturating_sub(1).max(1);
    }
    (next < current_chunk_rows).then_some(next)
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
    // The RV64 `#[provable]` wrapper reads two u32 values starting at RAM[0x108]:
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
    let proof_bytes = host.run(true).expect("proving should succeed");

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

    let result: ValueProofPublic = NightstreamVerifier::verify(&proof_bytes, &commitment)
        .expect("verification should succeed");

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
    use neo_fold::rv64_trace_shard::Rv64TraceWiring;
    use p3_field::{Field, PrimeCharacteristicRing, PrimeField64};
    use p3_goldilocks::Goldilocks;
    use sov_nightstream_adapter::{
        default_blacklist_root, BlacklistProof, NoteSpendInput, NoteSpendOutput, NoteSpendWitness,
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
        blacklist_proofs: vec![
            BlacklistProof::default_for_identity(&sender_id),
            BlacklistProof::default_for_identity(&sender_id),
        ],
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

    let ram_pairs = host.ram_init_pairs();

    // Mirror the Nightstream benchmark flow: simulate first, require halt,
    // then set proving bounds from the observed execution profile.
    let (executed_steps, sim_exec) = {
        let sim = simulate_rv64_elf(rom, &ram_pairs, 200_000);
        println!(
            "trace_sim_steps={} trace_sim_did_halt={} trace_sim_total_twist_events={} trace_sim_total_shout_events={}",
            sim.len(),
            sim.did_halt(),
            sim.total_twist_events(),
            sim.total_shout_events()
        );
        if !sim.did_halt() {
            let tail: Vec<(u64, u64, u32, bool)> = sim
                .steps
                .iter()
                .rev()
                .take(12)
                .map(|s| (s.pc_before, s.pc_after, s.opcode, s.halted))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            println!("trace_sim_tail={tail:?}");
            panic!("circuit did not halt within 200K simulation steps");
        }
        let exec = exec_table_from_sim(&sim);
        (sim.steps.len(), exec)
    };

    let chunk_rows_override = std::env::var("NS_NOTE_SPEND_CHUNK_ROWS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0);
    let requested_chunk_rows = chunk_rows_override
        .unwrap_or(executed_steps.min(NOTE_SPEND_TLEN_AUTOTUNE_CAP))
        .min(executed_steps)
        .max(1);
    let (chunk_rows, estimated_step_rows) = match chunk_rows_override {
        Some(_) => (
            requested_chunk_rows,
            effective_step_rows_for_requested(&sim_exec, requested_chunk_rows),
        ),
        None => auto_tune_chunk_rows(&sim_exec, requested_chunk_rows, NOTE_SPEND_TLEN_AUTOTUNE_CAP),
    };
    let no_output_binding = std::env::var_os("NS_PERF_NO_OUTPUT_BINDING").is_some();
    println!(
        "selected_chunk_rows={} selected_max_steps={} estimated_step_rows={} chunk_override={} no_output_binding={}",
        chunk_rows,
        executed_steps,
        estimated_step_rows,
        chunk_rows_override.is_some(),
        no_output_binding
    );

    let t_prove = Instant::now();
    let mut prove_chunk_rows = chunk_rows;
    let mut poseidon_retry_idx = 0usize;
    const MAX_POSEIDON_RETRIES: usize = 8;
    let mut run = loop {
        if no_output_binding {
            let mut wiring = Rv64TraceWiring::from_elf(&note_spend_rom::NOTE_SPEND_ROM)
                .expect("load RV64 guest")
            .chunk_rows(prove_chunk_rows)
            .max_steps(executed_steps);
            for &(addr, val) in &ram_pairs {
                wiring = wiring.ram_init_u32(addr, val);
            }
            match wiring.prove() {
                Ok(run) => break run,
                Err(err) => {
                    let msg = format!("{err:?}");
                    if msg.contains("poseidon-precompile feature is disabled") {
                        println!(
                            "Skipping note-spend integration test: poseidon-precompile is not enabled"
                        );
                        return;
                    }
                    if chunk_rows_override.is_none() && poseidon_retry_idx < MAX_POSEIDON_RETRIES {
                        if let Some(next_chunk_rows) =
                            poseidon_split_next_chunk_rows(&msg, prove_chunk_rows)
                        {
                            println!(
                                "note_spend: poseidon split retry chunk_rows {} -> {} (attempt {}/{})",
                                prove_chunk_rows,
                                next_chunk_rows,
                                poseidon_retry_idx + 1,
                                MAX_POSEIDON_RETRIES
                            );
                            prove_chunk_rows = next_chunk_rows;
                            poseidon_retry_idx += 1;
                            continue;
                        }
                    }
                    panic!("proving should succeed: {msg}");
                }
            }
        } else {
            host.set_chunk_rows(prove_chunk_rows);
            host.set_max_steps(executed_steps);
            match host.prove_run() {
                Ok(run) => break run,
                Err(err) => {
                    let msg = format!("{err:?}");
                    if msg.contains("poseidon-precompile feature is disabled") {
                        println!(
                            "Skipping note-spend integration test: poseidon-precompile is not enabled"
                        );
                        return;
                    }
                    if chunk_rows_override.is_none() && poseidon_retry_idx < MAX_POSEIDON_RETRIES {
                        if let Some(next_chunk_rows) =
                            poseidon_split_next_chunk_rows(&msg, prove_chunk_rows)
                        {
                            println!(
                                "note_spend: poseidon split retry chunk_rows {} -> {} (attempt {}/{})",
                                prove_chunk_rows,
                                next_chunk_rows,
                                poseidon_retry_idx + 1,
                                MAX_POSEIDON_RETRIES
                            );
                            prove_chunk_rows = next_chunk_rows;
                            poseidon_retry_idx += 1;
                            continue;
                        }
                    }
                    panic!("proving should succeed: {msg}");
                }
            }
        }
    };
    let prove_ms = t_prove.elapsed().as_millis();

    let t_verify = Instant::now();
    run.verify().expect("verification should not error");
    let verify_ms = t_verify.elapsed().as_millis();

    println!("\n=============================================");
    println!("  Note-Spend Prove+Verify (Valid Witness)");
    println!("=============================================");
    println!("  Proof generation: {} ms", prove_ms);
    println!("  Verification:     {} ms", verify_ms);
    println!("  RISC-V steps:     {}", run.trace_len());
    println!("  Folding steps:    {}", run.fold_count());
    println!("  CCS constraints:  {}", run.ccs_num_constraints());
    println!("  CCS variables:    {}", run.ccs_num_variables());
    let has_shout_events = run
        .exec_table()
        .rows
        .iter()
        .any(|row| !row.shout_events.is_empty());
    println!(
        "  Has shout events:  {}",
        has_shout_events
    );
    if std::env::var_os("NS_MEASURE_PROOF_SIZE").is_some() {
        if no_output_binding {
            use flate2::write::DeflateEncoder;
            use flate2::Compression;
            use std::io::Write;
            let raw = bincode::serialize(run.proof())
                .expect("serialize shard proof for size measurement");
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(&raw)
                .expect("write raw proof bytes for deflate");
            let compressed = encoder.finish().expect("finish deflate proof payload");
            println!(
                "  Proof payload:    {} bytes ({:.2} KB) [raw shard proof]",
                raw.len(),
                raw.len() as f64 / 1024.0
            );
            println!(
                "  Proof payload(z): {} bytes ({:.2} KB) [deflate shard proof]",
                compressed.len(),
                compressed.len() as f64 / 1024.0
            );
        } else {
            let t_pkg = Instant::now();
            let proof_bytes = host.run(true).expect("packaged proving should succeed");
            let pkg_ms = t_pkg.elapsed().as_millis();
            println!(
                "  Proof package:    {} bytes ({:.2} KB) [prove+package={} ms]",
                proof_bytes.len(),
                proof_bytes.len() as f64 / 1024.0,
                pkg_ms
            );
        }
    }
    assert_eq!(anchor, test_public.anchor_root);
    assert_eq!(vec![nullifier], test_public.nullifiers);
    assert_eq!(vec![out_cm], test_public.output_commitments);
    assert_eq!(blacklist_root, test_public.blacklist_root);
    println!("=============================================\n");
}

/// Full prove+verify cycle for the note-deposit circuit with a valid witness.
#[test]
fn test_note_deposit_prove_verify_with_witness() {
    use neo_ccs::crypto::poseidon2_goldilocks::poseidon2_hash;
    use neo_fold::rv64_trace_shard::Rv64TraceWiring;
    use p3_field::{PrimeCharacteristicRing, PrimeField64};
    use p3_goldilocks::Goldilocks;
    use sov_nightstream_adapter::{default_blacklist_root, BlacklistProof, NoteDepositWitness};

    type GlDigest = [Goldilocks; 4];

    const TAG_NOTE: u64 = 2;
    const TAG_ADDR: u64 = 5;

    fn gl_digest_to_bytes(d: &GlDigest) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, elem) in d.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&elem.as_canonical_u64().to_le_bytes());
        }
        out
    }

    // Domain + recipient keys
    let domain_gl = [Goldilocks::from_u64(1); 4];
    let domain = gl_digest_to_bytes(&domain_gl);
    let value: u64 = 777;

    let rho_gl = [
        Goldilocks::from_u64(200),
        Goldilocks::from_u64(201),
        Goldilocks::from_u64(202),
        Goldilocks::from_u64(203),
    ];
    let rho = gl_digest_to_bytes(&rho_gl);

    let pk_spend_gl = [
        Goldilocks::from_u64(42),
        Goldilocks::from_u64(43),
        Goldilocks::from_u64(44),
        Goldilocks::from_u64(45),
    ];
    let pk_spend_recipient = gl_digest_to_bytes(&pk_spend_gl);

    let pk_ivk_gl = [
        Goldilocks::from_u64(100),
        Goldilocks::from_u64(101),
        Goldilocks::from_u64(102),
        Goldilocks::from_u64(103),
    ];
    let pk_ivk_recipient = gl_digest_to_bytes(&pk_ivk_gl);

    // recipient = H(TAG_ADDR, domain, pk_spend, pk_ivk)
    let recipient_gl = {
        let mut inp = [Goldilocks::ZERO; 13];
        inp[0] = Goldilocks::from_u64(TAG_ADDR);
        inp[1..5].copy_from_slice(&domain_gl);
        inp[5..9].copy_from_slice(&pk_spend_gl);
        inp[9..13].copy_from_slice(&pk_ivk_gl);
        poseidon2_hash(&inp)
    };
    let recipient = gl_digest_to_bytes(&recipient_gl);

    // cm_out = H(TAG_NOTE, domain, value, rho, recipient, recipient)
    let cm_out_gl = {
        let mut inp = [Goldilocks::ZERO; 18];
        inp[0] = Goldilocks::from_u64(TAG_NOTE);
        inp[1..5].copy_from_slice(&domain_gl);
        inp[5] = Goldilocks::from_u64(value);
        inp[6..10].copy_from_slice(&rho_gl);
        inp[10..14].copy_from_slice(&recipient_gl);
        inp[14..18].copy_from_slice(&recipient_gl);
        poseidon2_hash(&inp)
    };
    let cm_out = gl_digest_to_bytes(&cm_out_gl);

    let blacklist_root = default_blacklist_root();
    let blacklist_proof = BlacklistProof::default_for_identity(&recipient);

    let witness = NoteDepositWitness {
        domain,
        value,
        rho,
        pk_spend_recipient,
        pk_ivk_recipient,
        cm_out,
        blacklist_root,
        blacklist_proof,
    };

    let mut host = NightstreamHost::new(
        &note_deposit_rom::NOTE_DEPOSIT_ROM,
        note_deposit_rom::NOTE_DEPOSIT_ROM_BASE,
    );
    host.write_note_deposit_witness(&witness);

    let ram_pairs = host.ram_init_pairs();
    let (executed_steps, sim_exec) = {
        let sim = simulate_rv64_elf(&note_deposit_rom::NOTE_DEPOSIT_ROM, &ram_pairs, 200_000);
        assert!(sim.did_halt(), "note-deposit circuit did not halt");
        let exec = exec_table_from_sim(&sim);
        (sim.steps.len(), exec)
    };

    let chunk_rows_override = std::env::var("NS_DEPOSIT_CHUNK_ROWS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0);
    let requested_chunk_rows = chunk_rows_override
        .unwrap_or(executed_steps.min(NOTE_DEPOSIT_TLEN_AUTOTUNE_CAP))
        .min(executed_steps)
        .max(1);
    let (chunk_rows, estimated_step_rows) = match chunk_rows_override {
        Some(_) => (
            requested_chunk_rows,
            effective_step_rows_for_requested(&sim_exec, requested_chunk_rows),
        ),
        None => auto_tune_chunk_rows(&sim_exec, requested_chunk_rows, NOTE_DEPOSIT_TLEN_AUTOTUNE_CAP),
    };
    println!(
        "note_deposit: selected_chunk_rows={} selected_max_steps={} estimated_step_rows={} chunk_override={}",
        chunk_rows,
        executed_steps,
        estimated_step_rows,
        chunk_rows_override.is_some()
    );
    let no_output_binding = std::env::var_os("NS_PERF_NO_OUTPUT_BINDING").is_some();

    let t_prove = Instant::now();
    let mut prove_chunk_rows = chunk_rows;
    let mut poseidon_retry_idx = 0usize;
    const MAX_POSEIDON_RETRIES: usize = 8;
    let mut run = loop {
        if no_output_binding {
            let mut wiring = Rv64TraceWiring::from_elf(&note_deposit_rom::NOTE_DEPOSIT_ROM)
                .expect("load RV64 guest")
            .chunk_rows(prove_chunk_rows)
            .max_steps(executed_steps);
            for &(addr, val) in &ram_pairs {
                wiring = wiring.ram_init_u32(addr, val);
            }
            match wiring.prove() {
                Ok(run) => break run,
                Err(err) => {
                    let msg = format!("{err:?}");
                    if chunk_rows_override.is_none() && poseidon_retry_idx < MAX_POSEIDON_RETRIES {
                        if let Some(next_chunk_rows) =
                            poseidon_split_next_chunk_rows(&msg, prove_chunk_rows)
                        {
                            println!(
                                "note_deposit: poseidon split retry chunk_rows {} -> {} (attempt {}/{})",
                                prove_chunk_rows,
                                next_chunk_rows,
                                poseidon_retry_idx + 1,
                                MAX_POSEIDON_RETRIES
                            );
                            prove_chunk_rows = next_chunk_rows;
                            poseidon_retry_idx += 1;
                            continue;
                        }
                    }
                    panic!("note-deposit proving should succeed: {msg}");
                }
            }
        } else {
            host.set_chunk_rows(prove_chunk_rows);
            host.set_max_steps(executed_steps);
            match host.prove_run() {
                Ok(run) => break run,
                Err(err) => {
                    let msg = format!("{err:?}");
                    if chunk_rows_override.is_none() && poseidon_retry_idx < MAX_POSEIDON_RETRIES {
                        if let Some(next_chunk_rows) =
                            poseidon_split_next_chunk_rows(&msg, prove_chunk_rows)
                        {
                            println!(
                                "note_deposit: poseidon split retry chunk_rows {} -> {} (attempt {}/{})",
                                prove_chunk_rows,
                                next_chunk_rows,
                                poseidon_retry_idx + 1,
                                MAX_POSEIDON_RETRIES
                            );
                            prove_chunk_rows = next_chunk_rows;
                            poseidon_retry_idx += 1;
                            continue;
                        }
                    }
                    panic!("note-deposit proving should succeed: {msg}");
                }
            }
        }
    };
    let prove_ms = t_prove.elapsed().as_millis();

    let t_verify = Instant::now();
    run.verify()
        .expect("note-deposit verification should not error");
    let verify_ms = t_verify.elapsed().as_millis();

    println!(
        "note_deposit: prove_ms={} verify_ms={} steps={} folds={} chunk_rows={} no_output_binding={}",
        prove_ms,
        verify_ms,
        run.trace_len(),
        run.fold_count(),
        prove_chunk_rows,
        no_output_binding
    );
    if std::env::var_os("NS_MEASURE_PROOF_SIZE").is_some() {
        if no_output_binding {
            use flate2::write::DeflateEncoder;
            use flate2::Compression;
            use std::io::Write;
            let raw = bincode::serialize(run.proof())
                .expect("serialize shard proof for size measurement");
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(&raw)
                .expect("write raw proof bytes for deflate");
            let compressed = encoder.finish().expect("finish deflate proof payload");
            println!(
                "note_deposit: proof_payload_bytes={} proof_payload_kb={:.2}",
                raw.len(),
                raw.len() as f64 / 1024.0
            );
            println!(
                "note_deposit: proof_payload_deflate_bytes={} proof_payload_deflate_kb={:.2}",
                compressed.len(),
                compressed.len() as f64 / 1024.0
            );
        } else {
            let t_pkg = Instant::now();
            let proof_bytes = host.run(true).expect("packaged proving should succeed");
            let pkg_ms = t_pkg.elapsed().as_millis();
            println!(
                "note_deposit: proof_package_bytes={} proof_package_kb={:.2} prove_plus_package_ms={}",
                proof_bytes.len(),
                proof_bytes.len() as f64 / 1024.0,
                pkg_ms
            );
        }
    }

    assert!(run.trace_len() > 0);
}

/// Replay a captured note-spend witness JSON through simulation/proving.
///
/// Usage:
/// `NS_WITNESS_JSON=/tmp/nightstream_note_spend_witness_fail_...json \
///    cargo test -p sov-nightstream-adapter --release --features native \
///    test_note_spend_replay_witness_json -- --ignored --nocapture`
#[test]
#[ignore = "debug helper for replaying captured /prove witness payloads"]
fn test_note_spend_replay_witness_json() {
    use neo_ccs::crypto::poseidon2_goldilocks::poseidon2_hash;
    use neo_fold::rv64_trace_shard::Rv64TraceWiring;
    use neo_memory::riscv::exec_table::RiscvExecTable;
    use neo_memory::riscv::lookups::RAM_ID;
    use p3_field::{PrimeCharacteristicRing, PrimeField64};
    use p3_goldilocks::Goldilocks;
    use sov_nightstream_adapter::{default_blacklist_root, NoteSpendWitness};

    let path = std::env::var("NS_WITNESS_JSON").expect("set NS_WITNESS_JSON to witness JSON path");
    let bytes = std::fs::read(&path).expect("read witness JSON");
    let witness: NoteSpendWitness = serde_json::from_slice(&bytes).expect("parse witness JSON");
    println!(
        "blacklist_root_is_default={}",
        witness.blacklist_root == default_blacklist_root()
    );

    type GlDigest = [Goldilocks; 4];
    const TAG_MT_NODE: u64 = 1;
    const TAG_NOTE: u64 = 2;
    const TAG_PRF_NF: u64 = 3;
    const TAG_PK: u64 = 4;
    const TAG_ADDR: u64 = 5;
    const TAG_NFKEY: u64 = 6;
    const TAG_BL_BUCKET: u64 = 7;
    const BL_DEPTH: usize = 16;
    const BL_BUCKET_SIZE: usize = 12;

    fn hash32_to_gl(h: &[u8; 32]) -> GlDigest {
        let mut d = [Goldilocks::ZERO; 4];
        for i in 0..4 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&h[i * 8..(i + 1) * 8]);
            d[i] = Goldilocks::from_u64(u64::from_le_bytes(buf));
        }
        d
    }

    fn gl_to_hash32(d: &GlDigest) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, elem) in d.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&elem.as_canonical_u64().to_le_bytes());
        }
        out
    }

    fn derive_pk_spend(spend_sk: &GlDigest) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 5];
        inp[0] = Goldilocks::from_u64(TAG_PK);
        inp[1..5].copy_from_slice(spend_sk);
        poseidon2_hash(&inp)
    }

    fn derive_nf_key(domain: &GlDigest, spend_sk: &GlDigest) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 9];
        inp[0] = Goldilocks::from_u64(TAG_NFKEY);
        inp[1..5].copy_from_slice(domain);
        inp[5..9].copy_from_slice(spend_sk);
        poseidon2_hash(&inp)
    }

    fn derive_address(domain: &GlDigest, pk_spend: &GlDigest, pk_ivk: &GlDigest) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 13];
        inp[0] = Goldilocks::from_u64(TAG_ADDR);
        inp[1..5].copy_from_slice(domain);
        inp[5..9].copy_from_slice(pk_spend);
        inp[9..13].copy_from_slice(pk_ivk);
        poseidon2_hash(&inp)
    }

    fn note_cm(
        domain: &GlDigest,
        value: u64,
        rho: &GlDigest,
        recipient: &GlDigest,
        sender_id: &GlDigest,
    ) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 18];
        inp[0] = Goldilocks::from_u64(TAG_NOTE);
        inp[1..5].copy_from_slice(domain);
        inp[5] = Goldilocks::from_u64(value);
        inp[6..10].copy_from_slice(rho);
        inp[10..14].copy_from_slice(recipient);
        inp[14..18].copy_from_slice(sender_id);
        poseidon2_hash(&inp)
    }

    fn derive_nullifier(domain: &GlDigest, nf_key: &GlDigest, rho: &GlDigest) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 13];
        inp[0] = Goldilocks::from_u64(TAG_PRF_NF);
        inp[1..5].copy_from_slice(domain);
        inp[5..9].copy_from_slice(nf_key);
        inp[9..13].copy_from_slice(rho);
        poseidon2_hash(&inp)
    }

    fn mt_node(level: u64, left: &GlDigest, right: &GlDigest) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 10];
        inp[0] = Goldilocks::from_u64(TAG_MT_NODE);
        inp[1] = Goldilocks::from_u64(level);
        inp[2..6].copy_from_slice(left);
        inp[6..10].copy_from_slice(right);
        poseidon2_hash(&inp)
    }

    fn merkle_root(leaf: &GlDigest, pos: u32, siblings: &[[u8; 32]], depth: u32) -> GlDigest {
        let mut cur = *leaf;
        let mut p = pos;
        for lvl in 0..depth as usize {
            let sib = hash32_to_gl(&siblings[lvl]);
            if (p & 1) == 0 {
                cur = mt_node(lvl as u64, &cur, &sib);
            } else {
                cur = mt_node(lvl as u64, &sib, &cur);
            }
            p >>= 1;
        }
        cur
    }

    fn enforce_prod_digest_diff(mut acc: Goldilocks, a: &GlDigest, b: &GlDigest) -> Goldilocks {
        for i in 0..4 {
            acc *= a[i] - b[i];
        }
        acc
    }

    fn bl_bucket_leaf(entries: &[[u8; 32]; BL_BUCKET_SIZE]) -> GlDigest {
        let mut inp = [Goldilocks::ZERO; 1 + BL_BUCKET_SIZE * 4];
        inp[0] = Goldilocks::from_u64(TAG_BL_BUCKET);
        for (i, e) in entries.iter().enumerate() {
            let gl = hash32_to_gl(e);
            let off = 1 + i * 4;
            inp[off..off + 4].copy_from_slice(&gl);
        }
        poseidon2_hash(&inp)
    }

    fn bl_bucket_pos(id: &GlDigest) -> u32 {
        let bytes = gl_to_hash32(id);
        let mut pos = 0u32;
        for i in 0..BL_DEPTH {
            let byte = bytes[31 - (i / 8)];
            let bit = (byte >> (i % 8)) & 1;
            pos |= (bit as u32) << (i as u32);
        }
        pos
    }

    let domain_gl = hash32_to_gl(&witness.domain);
    let spend_sk_gl = hash32_to_gl(&witness.spend_sk);
    let pk_ivk_owner_gl = hash32_to_gl(&witness.pk_ivk_owner);
    let pk_spend_owner_gl = derive_pk_spend(&spend_sk_gl);
    let nf_key_gl = derive_nf_key(&domain_gl, &spend_sk_gl);
    let recipient_owner_gl = derive_address(&domain_gl, &pk_spend_owner_gl, &pk_ivk_owner_gl);
    let sender_id_out_gl = recipient_owner_gl;
    let sender_id_out = gl_to_hash32(&sender_id_out_gl);
    let mut input_anchor_ok = true;
    let mut input_nf_ok = true;
    let mut in_values = Vec::new();
    let mut in_rhos = Vec::new();
    let mut sum_in: u64 = 0;
    for (idx, input) in witness.inputs.iter().enumerate() {
        let rho_gl = hash32_to_gl(&input.rho);
        let sender_gl = hash32_to_gl(&input.sender_id);
        let cm_gl = note_cm(
            &domain_gl,
            input.value,
            &rho_gl,
            &recipient_owner_gl,
            &sender_gl,
        );
        let root_gl = merkle_root(&cm_gl, input.position, &input.siblings, witness.depth);
        let anchor_ok = gl_to_hash32(&root_gl) == witness.anchor;
        let nf_gl = derive_nullifier(&domain_gl, &nf_key_gl, &rho_gl);
        let nf_ok = gl_to_hash32(&nf_gl) == input.nullifier;
        println!(
            "replay_input[{idx}] anchor_ok={} nullifier_ok={} pos={} siblings={}",
            anchor_ok,
            nf_ok,
            input.position,
            input.siblings.len()
        );
        input_anchor_ok &= anchor_ok;
        input_nf_ok &= nf_ok;
        in_values.push(input.value);
        in_rhos.push(rho_gl);
        sum_in = sum_in.wrapping_add(input.value);
    }
    println!(
        "replay_inputs_anchor_all_ok={} replay_inputs_nullifier_all_ok={}",
        input_anchor_ok, input_nf_ok
    );

    let mut output_cm_ok = true;
    let mut out_values = Vec::new();
    let mut out_rhos = Vec::new();
    let mut out_recipients = Vec::new();
    let mut sum_out: u64 = 0;
    for (j, output) in witness.outputs.iter().enumerate() {
        let out_rho_gl = hash32_to_gl(&output.rho);
        let pk_spend_gl = hash32_to_gl(&output.pk_spend);
        let pk_ivk_gl = hash32_to_gl(&output.pk_ivk);
        let rcp_gl = derive_address(&domain_gl, &pk_spend_gl, &pk_ivk_gl);
        let cm_gl = note_cm(
            &domain_gl,
            output.value,
            &out_rho_gl,
            &rcp_gl,
            &sender_id_out_gl,
        );
        let cm_ok = gl_to_hash32(&cm_gl) == output.cm;
        println!("replay_output[{j}] cm_ok={cm_ok}");
        output_cm_ok &= cm_ok;
        sum_out = sum_out.wrapping_add(output.value);
        out_values.push(output.value);
        out_rhos.push(out_rho_gl);
        out_recipients.push(gl_to_hash32(&rcp_gl));
    }
    let balance_ok = sum_in == witness.withdraw_amount.wrapping_add(sum_out);
    println!("replay_balance_ok={balance_ok}");

    let mut enforce_prod = Goldilocks::ONE;
    for v in &in_values {
        enforce_prod *= Goldilocks::from_u64(*v);
    }
    for v in &out_values {
        enforce_prod *= Goldilocks::from_u64(*v);
    }
    for out_rho in &out_rhos {
        for in_rho in &in_rhos {
            enforce_prod = enforce_prod_digest_diff(enforce_prod, out_rho, in_rho);
        }
    }
    if out_rhos.len() == 2 {
        enforce_prod = enforce_prod_digest_diff(enforce_prod, &out_rhos[0], &out_rhos[1]);
    }
    let mut inv_buf = [0u8; 8];
    inv_buf.copy_from_slice(&witness.inv_enforce[..8]);
    let inv_enforce_u64 = u64::from_le_bytes(inv_buf);
    let enforce_ok = enforce_prod * Goldilocks::from_u64(inv_enforce_u64) == Goldilocks::ONE;
    println!("replay_enforce_ok={enforce_ok}");

    let mut bl_ok = true;
    let mut ids_to_check: Vec<[u8; 32]> = vec![sender_id_out];
    if witness.withdraw_amount == 0 {
        if let Some(rcp0) = out_recipients.first() {
            ids_to_check.push(*rcp0);
        }
    }
    if ids_to_check.len() != witness.blacklist_proofs.len() {
        bl_ok = false;
        println!(
            "replay_blacklist_shape_mismatch ids_to_check={} proofs={}",
            ids_to_check.len(),
            witness.blacklist_proofs.len()
        );
    }
    for (k, (id, proof)) in ids_to_check
        .iter()
        .zip(witness.blacklist_proofs.iter())
        .enumerate()
    {
        let id_gl = hash32_to_gl(id);
        let mut prod = Goldilocks::ONE;
        for e in &proof.bucket_entries {
            let e_gl = hash32_to_gl(e);
            prod = enforce_prod_digest_diff(prod, &id_gl, &e_gl);
        }
        let mut inv_b = [0u8; 8];
        inv_b.copy_from_slice(&proof.bucket_inv[..8]);
        let inv_u64 = u64::from_le_bytes(inv_b);
        let inv_ok = prod * Goldilocks::from_u64(inv_u64) == Goldilocks::ONE;

        let leaf = bl_bucket_leaf(&proof.bucket_entries);
        let pos = bl_bucket_pos(&id_gl);
        let root = merkle_root(&leaf, pos, &proof.siblings, BL_DEPTH as u32);
        let root_ok = gl_to_hash32(&root) == witness.blacklist_root;
        println!(
            "replay_blacklist[{k}] inv_ok={} root_ok={}",
            inv_ok, root_ok
        );
        bl_ok &= inv_ok && root_ok;
    }
    println!(
        "replay_summary output_cm_ok={} balance_ok={} enforce_ok={} blacklist_ok={}",
        output_cm_ok, balance_ok, enforce_ok, bl_ok
    );

    let mut host = NightstreamHost::new(
        &note_spend_rom::NOTE_SPEND_ROM,
        note_spend_rom::NOTE_SPEND_ROM_BASE,
    );
    host.set_output_binding_enabled(false);
    host.write_note_spend_witness(&witness, vec![]);

    let ram_pairs = host.ram_init_pairs();
    let ram_preview: Vec<(u64, u32)> = ram_pairs
        .iter()
        .copied()
        .filter(|(addr, _)| *addr >= 0x100 && *addr <= 0x140)
        .collect();
    println!("replay_ram_preview={ram_preview:?}");
    let sim = simulate_rv64_elf(&note_spend_rom::NOTE_SPEND_ROM, &ram_pairs, 300_000);
    let debug_addr: u64 = 0x4090;
    let debug_writes = sim
        .steps
        .iter()
        .flat_map(|s| s.twist_events.iter())
        .filter(|ev| {
            ev.twist_id == RAM_ID
                && ev.kind == neo_vm_trace::TwistOpKind::Write
                && ev.addr == debug_addr
        })
        .map(|ev| ev.value)
        .collect::<Vec<_>>();
    if let Some(last) = debug_writes.last() {
        println!(
            "replay_debug_stage_writes={} last_stage={} (addr=0x{debug_addr:08x})",
            debug_writes.len(),
            last
        );
    } else {
        println!("replay_debug_stage_writes=0 (addr=0x{debug_addr:08x})");
    }

    println!(
        "replay_witness={} steps={} did_halt={} total_twist_events={} total_shout_events={}",
        path,
        sim.len(),
        sim.did_halt(),
        sim.total_twist_events(),
        sim.total_shout_events()
    );
    if !sim.did_halt() {
        let tail: Vec<(u64, u64, u32, bool)> = sim
            .steps
            .iter()
            .rev()
            .take(16)
            .map(|s| (s.pc_before, s.pc_after, s.opcode, s.halted))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        println!("trace_sim_tail={tail:?}");
    }

    let replay_prove = std::env::var("NS_REPLAY_PROVE")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if replay_prove {
        use neo_math::F;
        use neo_vm_trace::TwistOpKind;
        use p3_field::PrimeCharacteristicRing;
        use std::collections::{BTreeSet, HashMap};

        fn derive_output_claims_for_addresses(
            exec: &RiscvExecTable,
            ram_pairs: &[(u64, u32)],
            output_addrs: &[u64],
        ) -> Vec<(u64, u32)> {
            let output_addr_set: BTreeSet<u64> = output_addrs.iter().copied().collect();
            let mut final_output_values: HashMap<u64, u32> =
                output_addr_set.iter().map(|&addr| (addr, 0u32)).collect();
            for &(addr, value) in ram_pairs {
                if output_addr_set.contains(&addr) {
                    final_output_values.insert(addr, value);
                }
            }
            for row in exec.rows.iter().filter(|r| r.active) {
                for ev in &row.ram_events {
                    if ev.kind == TwistOpKind::Write && output_addr_set.contains(&ev.addr) {
                        final_output_values.insert(ev.addr, ev.value as u32);
                    }
                }
            }
            output_addrs
                .iter()
                .map(|addr| (*addr, *final_output_values.get(addr).unwrap_or(&0u32)))
                .collect()
        }

        fn push_u32_claim(out: &mut Vec<(u64, u32)>, addr: &mut u64, val: u32) {
            out.push((*addr, val));
            *addr += 4;
        }

        fn push_u64_claim(out: &mut Vec<(u64, u32)>, addr: &mut u64, val: u64) {
            push_u32_claim(out, addr, val as u32);
            push_u32_claim(out, addr, (val >> 32) as u32);
        }

        fn push_digest_claim(out: &mut Vec<(u64, u32)>, addr: &mut u64, d: &[u8; 32]) {
            for i in 0..4 {
                let mut lo = [0u8; 4];
                let mut hi = [0u8; 4];
                lo.copy_from_slice(&d[i * 8..i * 8 + 4]);
                hi.copy_from_slice(&d[i * 8 + 4..i * 8 + 8]);
                push_u32_claim(out, addr, u32::from_le_bytes(lo));
                push_u32_claim(out, addr, u32::from_le_bytes(hi));
            }
        }

        let mut template_output_claims: Vec<(u64, u32)> = Vec::new();
        let mut out_addr: u64 = 0x100;
        push_digest_claim(&mut template_output_claims, &mut out_addr, &witness.anchor);
        push_u32_claim(
            &mut template_output_claims,
            &mut out_addr,
            witness.inputs.len() as u32,
        );
        for input in &witness.inputs {
            push_digest_claim(&mut template_output_claims, &mut out_addr, &input.nullifier);
        }
        push_u64_claim(
            &mut template_output_claims,
            &mut out_addr,
            witness.withdraw_amount,
        );
        push_digest_claim(
            &mut template_output_claims,
            &mut out_addr,
            &witness.withdraw_to,
        );
        push_u32_claim(
            &mut template_output_claims,
            &mut out_addr,
            witness.outputs.len() as u32,
        );
        for output in &witness.outputs {
            push_digest_claim(&mut template_output_claims, &mut out_addr, &output.cm);
        }
        push_digest_claim(
            &mut template_output_claims,
            &mut out_addr,
            &witness.blacklist_root,
        );
        push_u32_claim(
            &mut template_output_claims,
            &mut out_addr,
            witness.viewers.len() as u32,
        );
        for viewer in &witness.viewers {
            for (out_w, output) in viewer.per_output.iter().zip(&witness.outputs) {
                push_digest_claim(&mut template_output_claims, &mut out_addr, &output.cm);
                push_digest_claim(
                    &mut template_output_claims,
                    &mut out_addr,
                    &viewer.fvk_commitment,
                );
                push_digest_claim(&mut template_output_claims, &mut out_addr, &out_w.ct_hash);
                push_digest_claim(&mut template_output_claims, &mut out_addr, &out_w.mac);
            }
        }
        let output_addrs: Vec<u64> = template_output_claims
            .iter()
            .map(|(addr, _)| *addr)
            .collect();
        let sim_exec = exec_table_from_sim(&sim);
        let output_min = output_addrs.iter().copied().min().unwrap_or(0);
        let output_max = output_addrs.iter().copied().max().unwrap_or(0);
        let output_writes = sim_exec
            .rows
            .iter()
            .filter(|r| r.active)
            .flat_map(|r| r.ram_events.iter())
            .filter(|ev| {
                ev.kind == TwistOpKind::Write && ev.addr >= output_min && ev.addr <= output_max
            })
            .map(|ev| (ev.addr, ev.value as u32))
            .collect::<Vec<_>>();
        println!(
            "replay_sim_output_window=[0x{output_min:08x}..=0x{output_max:08x}] writes_in_window={}",
            output_writes.len()
        );
        for (idx, (addr, val)) in output_writes.iter().take(16).enumerate() {
            println!("replay_sim_output_write[{idx}]=0x{addr:08x} value=0x{val:08x}");
        }
        let sim_observed = derive_output_claims_for_addresses(&sim_exec, &ram_pairs, &output_addrs);
        let sim_observed_map: HashMap<u64, u32> = sim_observed.iter().copied().collect();
        let sim_mismatches = template_output_claims
            .iter()
            .filter_map(|(addr, expected)| {
                let actual = *sim_observed_map.get(addr).unwrap_or(&0u32);
                if actual == *expected {
                    None
                } else {
                    Some((*addr, *expected, actual))
                }
            })
            .collect::<Vec<_>>();
        println!(
            "replay_sim_claims={} mismatches={}",
            template_output_claims.len(),
            sim_mismatches.len()
        );
        if let Some((addr, expected, actual)) = sim_mismatches.first() {
            println!(
                "replay_sim_first_mismatch=0x{addr:08x} expected=0x{expected:08x} actual=0x{actual:08x}"
            );
        }
        for (idx, (addr, expected, actual)) in sim_mismatches.iter().take(8).enumerate() {
            println!(
                "replay_sim_mismatch[{idx}]=0x{addr:08x} expected=0x{expected:08x} actual=0x{actual:08x}"
            );
        }

        let chunk_rows = std::env::var("NS_REPLAY_CHUNK_ROWS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(510usize)
            .max(1);
        let skip_wiring = std::env::var("NS_REPLAY_SKIP_WIRING")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        if !skip_wiring {
            let mut wiring = Rv64TraceWiring::from_elf(&note_spend_rom::NOTE_SPEND_ROM)
                .expect("load RV64 guest")
            .chunk_rows(chunk_rows)
            .max_steps(sim.steps.len());
            for &(addr, val) in &ram_pairs {
                wiring = wiring.ram_init_u32(addr, val);
            }
            for &(addr, val) in &template_output_claims {
                wiring = wiring.output_claim(addr, F::from_u64(val as u64));
            }
            let t_wiring = std::time::Instant::now();
            let mut wiring_run = wiring.prove().expect("wiring prove with template claims");
            let wiring_ms = t_wiring.elapsed().as_millis();
            let wiring_verify = wiring_run.verify();
            let observed = derive_output_claims_for_addresses(
                wiring_run.exec_table(),
                &ram_pairs,
                &output_addrs,
            );
            let observed_map: HashMap<u64, u32> = observed.iter().copied().collect();
            let mismatches = template_output_claims
                .iter()
                .filter_map(|(addr, expected)| {
                    let actual = *observed_map.get(addr).unwrap_or(&0u32);
                    if actual == *expected {
                        None
                    } else {
                        Some((*addr, *expected, actual))
                    }
                })
                .collect::<Vec<_>>();
            println!(
                "replay_wiring_prove_ms={} claims={} mismatches={} verify_ok={}",
                wiring_ms,
                template_output_claims.len(),
                mismatches.len(),
                wiring_verify.is_ok()
            );
            if let Some((addr, expected, actual)) = mismatches.first() {
                println!(
                    "replay_wiring_first_mismatch=0x{addr:08x} expected=0x{expected:08x} actual=0x{actual:08x}"
                );
            }
            for (idx, (addr, expected, actual)) in mismatches.iter().take(8).enumerate() {
                println!(
                    "replay_wiring_mismatch[{idx}]=0x{addr:08x} expected=0x{expected:08x} actual=0x{actual:08x}"
                );
            }
        } else {
            println!("replay_wiring_skipped=true");
        }

        let mut prove_host = NightstreamHost::new(
            &note_spend_rom::NOTE_SPEND_ROM,
            note_spend_rom::NOTE_SPEND_ROM_BASE,
        );
        let replay_no_output_binding = std::env::var("NS_REPLAY_NO_OUTPUT_BINDING")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        if replay_no_output_binding {
            prove_host.set_output_binding_enabled(false);
        }
        prove_host.set_chunk_rows(chunk_rows.max(1));
        let replay_set_max_steps = std::env::var("NS_REPLAY_SET_MAX_STEPS")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(true);
        if replay_set_max_steps {
            prove_host.set_max_steps(sim.steps.len());
        }
        prove_host.write_note_spend_witness(&witness, vec![]);
        let t = std::time::Instant::now();
        match prove_host.run(true) {
            Ok(proof) => {
                println!(
                    "replay_prove_ok=true chunk_rows={} no_output_binding={} set_max_steps={} proof_bytes={} elapsed_ms={}",
                    chunk_rows,
                    replay_no_output_binding,
                    replay_set_max_steps,
                    proof.len(),
                    t.elapsed().as_millis()
                );
            }
            Err(err) => {
                println!(
                    "replay_prove_ok=false chunk_rows={} no_output_binding={} set_max_steps={} elapsed_ms={} error={}",
                    chunk_rows,
                    replay_no_output_binding,
                    replay_set_max_steps,
                    t.elapsed().as_millis(),
                    err
                );
            }
        }
    }
}

/// Test prove+verify cycle twice with different inputs using trace wiring.
///
/// Uses the value-validator circuit for speed.
#[test]
fn test_prove_and_verify_trace_wiring() {
    let rom_bytes = &value_validator_rom::VALUE_VALIDATOR_ROM;
    let program_base = value_validator_rom::VALUE_VALIDATOR_ROM_BASE;

    // --- First prove+verify ---
    let mut host1 = NightstreamHost::new(rom_bytes, program_base);
    host1.add_u32_input(42);
    host1.add_u32_input(42);
    host1.add_output_claim(0x100, 42);

    let t1_prove = Instant::now();
    let compressed1 = host1.run(true).expect("first prove should succeed");
    let prove1_ms = t1_prove.elapsed().as_millis();

    let package1 = decompress_package(&compressed1);

    let t1_verify = Instant::now();
    let ok1 = package1.verify().expect("first verify should not error");
    assert!(ok1, "first verification should pass");
    let verify1_ms = t1_verify.elapsed().as_millis();

    // --- Second prove+verify (different input) ---
    let mut host2 = NightstreamHost::new(rom_bytes, program_base);
    host2.add_u32_input(99);
    host2.add_u32_input(99);
    host2.add_output_claim(0x100, 99);

    let t2_prove = Instant::now();
    let compressed2 = host2.run(true).expect("second prove should succeed");
    let prove2_ms = t2_prove.elapsed().as_millis();

    let package2 = decompress_package(&compressed2);

    let t2_verify = Instant::now();
    let ok2 = package2.verify().expect("second verify should not error");
    assert!(ok2, "second verification should pass");
    let verify2_ms = t2_verify.elapsed().as_millis();

    println!("\n=============================================");
    println!("  Trace Wiring Test (value-validator)");
    println!("=============================================");
    println!("  1st prove:  {} ms", prove1_ms);
    println!("  1st verify: {} ms", verify1_ms);
    println!("  2nd prove:  {} ms", prove2_ms);
    println!("  2nd verify: {} ms", verify2_ms);
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
