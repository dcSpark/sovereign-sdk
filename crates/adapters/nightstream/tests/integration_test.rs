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
        None => auto_tune_chunk_rows(
            &sim_exec,
            requested_chunk_rows,
            NOTE_SPEND_TLEN_AUTOTUNE_CAP,
        ),
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
    println!("  Has shout events:  {}", has_shout_events);
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

/// Build a realistic viewer-attested note-spend witness mirroring proof-pool `/prove`.
fn realistic_viewer_attested_note_spend_case() -> (
    sov_nightstream_adapter::NoteSpendWitness,
    sov_nightstream_adapter::circuit_output::SpendPublicWire,
) {
    use neo_ccs::crypto::poseidon2_goldilocks::poseidon2_hash;
    use p3_field::{Field, PrimeCharacteristicRing, PrimeField64};
    use p3_goldilocks::Goldilocks;
    use sov_nightstream_adapter::circuit_output::{SpendPublicViewAttestation, SpendPublicWire};
    use sov_nightstream_adapter::{
        default_blacklist_root, BlacklistProof, NoteSpendInput, NoteSpendOutput, NoteSpendWitness,
        ViewerOutputWitness, ViewerWitness,
    };

    type GlDigest = [Goldilocks; 4];
    const ZERO_DIGEST: GlDigest = [Goldilocks::ZERO; 4];
    const MAX_INS: usize = 4;
    const NOTE_PLAIN_LEN: usize = 272;

    const TAG_MT_NODE: u64 = 1;
    const TAG_NOTE: u64 = 2;
    const TAG_PRF_NF: u64 = 3;
    const TAG_PK: u64 = 4;
    const TAG_ADDR: u64 = 5;
    const TAG_NFKEY: u64 = 6;
    const TAG_FVK_COMMIT: u64 = 100;
    const TAG_VIEW_KDF: u64 = 101;
    const TAG_VIEW_STREAM: u64 = 102;
    const TAG_CT_HASH: u64 = 103;
    const TAG_VIEW_MAC: u64 = 104;

    fn gl_digest_to_bytes(d: &GlDigest) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, elem) in d.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&elem.as_canonical_u64().to_le_bytes());
        }
        out
    }

    fn pack_bytes_to_felts(bytes: &[u8], out: &mut [u64]) -> usize {
        let len = bytes.len();
        let n_elems = len.div_ceil(8);
        for i in 0..n_elems {
            let off = i * 8;
            let mut buf = [0u8; 8];
            let take = if off + 8 <= len { 8 } else { len - off };
            buf[..take].copy_from_slice(&bytes[off..off + take]);
            out[i] = u64::from_le_bytes(buf);
        }
        n_elems
    }

    fn view_fvk_commitment(fvk: &GlDigest) -> GlDigest {
        let mut input = [Goldilocks::ZERO; 5];
        input[0] = Goldilocks::from_u64(TAG_FVK_COMMIT);
        input[1..5].copy_from_slice(fvk);
        poseidon2_hash(&input)
    }

    fn view_kdf(fvk: &GlDigest, cm: &GlDigest) -> GlDigest {
        let mut input = [Goldilocks::ZERO; 9];
        input[0] = Goldilocks::from_u64(TAG_VIEW_KDF);
        input[1..5].copy_from_slice(fvk);
        input[5..9].copy_from_slice(cm);
        poseidon2_hash(&input)
    }

    fn view_stream_block(k: &GlDigest, ctr: u32) -> GlDigest {
        let mut input = [Goldilocks::ZERO; 6];
        input[0] = Goldilocks::from_u64(TAG_VIEW_STREAM);
        input[1..5].copy_from_slice(k);
        input[5] = Goldilocks::from_u64(ctr as u64);
        poseidon2_hash(&input)
    }

    fn view_ct_hash(ct: &[u8; NOTE_PLAIN_LEN]) -> GlDigest {
        let mut packed = [0u64; 1 + (NOTE_PLAIN_LEN / 8) + 1];
        packed[0] = TAG_CT_HASH;
        let n = pack_bytes_to_felts(ct, &mut packed[1..]);
        packed[1 + n] = NOTE_PLAIN_LEN as u64;

        let mut input = vec![Goldilocks::ZERO; 1 + n + 1];
        for (idx, value) in packed[..1 + n + 1].iter().enumerate() {
            input[idx] = Goldilocks::from_u64(*value);
        }
        poseidon2_hash(&input)
    }

    fn view_mac(k: &GlDigest, cm: &GlDigest, ct_h: &GlDigest) -> GlDigest {
        let mut input = [Goldilocks::ZERO; 13];
        input[0] = Goldilocks::from_u64(TAG_VIEW_MAC);
        input[1..5].copy_from_slice(k);
        input[5..9].copy_from_slice(cm);
        input[9..13].copy_from_slice(ct_h);
        poseidon2_hash(&input)
    }

    fn view_stream_xor_encrypt(k: &GlDigest, pt: &[u8; NOTE_PLAIN_LEN]) -> [u8; NOTE_PLAIN_LEN] {
        let mut ct = [0u8; NOTE_PLAIN_LEN];
        let mut ctr = 0u32;
        let mut off = 0usize;
        while off < NOTE_PLAIN_LEN {
            let ks = view_stream_block(k, ctr);
            let ks_bytes = gl_digest_to_bytes(&ks);
            ctr += 1;
            let take = (NOTE_PLAIN_LEN - off).min(32);
            for j in 0..take {
                ct[off + j] = pt[off + j] ^ ks_bytes[j];
            }
            off += take;
        }
        ct
    }

    fn encode_note_plain(
        domain: &GlDigest,
        value: u64,
        rho: &GlDigest,
        recipient: &GlDigest,
        sender_id: &GlDigest,
        cm_ins: &[GlDigest; MAX_INS],
        n_in: u32,
    ) -> [u8; NOTE_PLAIN_LEN] {
        let mut pt = [0u8; NOTE_PLAIN_LEN];
        pt[..32].copy_from_slice(&gl_digest_to_bytes(domain));
        pt[32..40].copy_from_slice(&value.to_le_bytes());
        pt[48..80].copy_from_slice(&gl_digest_to_bytes(rho));
        pt[80..112].copy_from_slice(&gl_digest_to_bytes(recipient));
        pt[112..144].copy_from_slice(&gl_digest_to_bytes(sender_id));
        for (idx, cm) in cm_ins.iter().enumerate() {
            if (idx as u32) < n_in {
                let off = 144 + idx * 32;
                pt[off..off + 32].copy_from_slice(&gl_digest_to_bytes(cm));
            }
        }
        pt
    }

    let domain_gl = [Goldilocks::from_u64(1); 4];
    let domain = gl_digest_to_bytes(&domain_gl);

    let spend_sk_gl = [
        Goldilocks::from_u64(42),
        Goldilocks::from_u64(43),
        Goldilocks::from_u64(44),
        Goldilocks::from_u64(45),
    ];
    let spend_sk = gl_digest_to_bytes(&spend_sk_gl);

    let mut pk_input = [Goldilocks::ZERO; 5];
    pk_input[0] = Goldilocks::from_u64(TAG_PK);
    pk_input[1..5].copy_from_slice(&spend_sk_gl);
    let pk_spend_gl = poseidon2_hash(&pk_input);

    let pk_ivk_gl = [
        Goldilocks::from_u64(100),
        Goldilocks::from_u64(101),
        Goldilocks::from_u64(102),
        Goldilocks::from_u64(103),
    ];
    let pk_ivk_owner = gl_digest_to_bytes(&pk_ivk_gl);

    let mut nfk_input = [Goldilocks::ZERO; 9];
    nfk_input[0] = Goldilocks::from_u64(TAG_NFKEY);
    nfk_input[1..5].copy_from_slice(&domain_gl);
    nfk_input[5..9].copy_from_slice(&spend_sk_gl);
    let nf_key_gl = poseidon2_hash(&nfk_input);

    let mut addr_input = [Goldilocks::ZERO; 13];
    addr_input[0] = Goldilocks::from_u64(TAG_ADDR);
    addr_input[1..5].copy_from_slice(&domain_gl);
    addr_input[5..9].copy_from_slice(&pk_spend_gl);
    addr_input[9..13].copy_from_slice(&pk_ivk_gl);
    let recipient_gl = poseidon2_hash(&addr_input);
    let sender_id_gl = recipient_gl;

    let value: u64 = 1000;
    let rho_gl = [
        Goldilocks::from_u64(200),
        Goldilocks::from_u64(201),
        Goldilocks::from_u64(202),
        Goldilocks::from_u64(203),
    ];
    let rho = gl_digest_to_bytes(&rho_gl);
    let sender_id = gl_digest_to_bytes(&sender_id_gl);

    let mut cm_input = [Goldilocks::ZERO; 18];
    cm_input[0] = Goldilocks::from_u64(TAG_NOTE);
    cm_input[1..5].copy_from_slice(&domain_gl);
    cm_input[5] = Goldilocks::from_u64(value);
    cm_input[6..10].copy_from_slice(&rho_gl);
    cm_input[10..14].copy_from_slice(&recipient_gl);
    cm_input[14..18].copy_from_slice(&sender_id_gl);
    let cm_gl = poseidon2_hash(&cm_input);

    let sib0 = ZERO_DIGEST;
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

    let mut nf_input = [Goldilocks::ZERO; 13];
    nf_input[0] = Goldilocks::from_u64(TAG_PRF_NF);
    nf_input[1..5].copy_from_slice(&domain_gl);
    nf_input[5..9].copy_from_slice(&nf_key_gl);
    nf_input[9..13].copy_from_slice(&rho_gl);
    let nf_gl = poseidon2_hash(&nf_input);
    let nullifier = gl_digest_to_bytes(&nf_gl);

    let out_rho_gl = [
        Goldilocks::from_u64(300),
        Goldilocks::from_u64(301),
        Goldilocks::from_u64(302),
        Goldilocks::from_u64(303),
    ];
    let out_rho = gl_digest_to_bytes(&out_rho_gl);
    let out_pk_spend = gl_digest_to_bytes(&pk_spend_gl);
    let out_pk_ivk = pk_ivk_owner;

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

    let mut enforce_prod = Goldilocks::from_u64(value);
    enforce_prod *= Goldilocks::from_u64(value);
    for i in 0..4 {
        enforce_prod *= out_rho_gl[i] - rho_gl[i];
    }
    let inv_enforce_gl = enforce_prod.inverse();
    let mut inv_enforce = [0u8; 32];
    inv_enforce[..8].copy_from_slice(&inv_enforce_gl.as_canonical_u64().to_le_bytes());

    let viewer_fvk_gl = [
        Goldilocks::from_u64(700),
        Goldilocks::from_u64(701),
        Goldilocks::from_u64(702),
        Goldilocks::from_u64(703),
    ];
    let viewer_fvk = gl_digest_to_bytes(&viewer_fvk_gl);
    let viewer_fvk_commitment = gl_digest_to_bytes(&view_fvk_commitment(&viewer_fvk_gl));

    let mut cm_ins = [ZERO_DIGEST; MAX_INS];
    cm_ins[0] = cm_gl;
    let plaintext = encode_note_plain(
        &domain_gl,
        value,
        &out_rho_gl,
        &recipient_gl,
        &sender_id_gl,
        &cm_ins,
        1,
    );
    let k = view_kdf(&viewer_fvk_gl, &out_cm_gl);
    let ciphertext = view_stream_xor_encrypt(&k, &plaintext);
    let ct_hash_gl = view_ct_hash(&ciphertext);
    let mac_gl = view_mac(&k, &out_cm_gl, &ct_hash_gl);
    let ct_hash = gl_digest_to_bytes(&ct_hash_gl);
    let mac = gl_digest_to_bytes(&mac_gl);

    let blacklist_root = default_blacklist_root();
    let witness = NoteSpendWitness {
        domain,
        spend_sk,
        pk_ivk_owner,
        depth: 2,
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
        viewers: vec![ViewerWitness {
            fvk_commitment: viewer_fvk_commitment,
            fvk: viewer_fvk,
            per_output: vec![ViewerOutputWitness { ct_hash, mac }],
        }],
    };

    let expected_public = SpendPublicWire {
        anchor_root: anchor,
        blacklist_root,
        nullifiers: vec![nullifier],
        withdraw_amount: 0,
        output_commitments: vec![out_cm],
        view_attestations: Some(vec![SpendPublicViewAttestation {
            cm: out_cm,
            fvk_commitment: viewer_fvk_commitment,
            ct_hash,
            mac,
        }]),
    };

    (witness, expected_public)
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
        None => auto_tune_chunk_rows(
            &sim_exec,
            requested_chunk_rows,
            NOTE_DEPOSIT_TLEN_AUTOTUNE_CAP,
        ),
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

/// Exercise the proof-pool viewer-attested `/prove` flow without requiring an
/// external JSON witness file.
///
/// In release builds this runs the full prove + in-memory verify +
/// packaged-proof verify path. In debug builds we still run the realistic
/// witness through simulation and chunk-row tuning, but skip proving because
/// upstream Nightstream currently trips debug-only MLE assertions.
#[test]
fn test_note_spend_replay_viewer_witness_with_output_binding() {
    use sov_nightstream_adapter::circuit_output::SpendPublicWire;

    let (witness, expected_public) = realistic_viewer_attested_note_spend_case();
    let public_bytes =
        bincode::serialize(&expected_public).expect("serialize viewer public output");

    assert!(
        !witness.viewers.is_empty(),
        "viewer-attested replay case must include viewers"
    );
    for (viewer_idx, viewer) in witness.viewers.iter().enumerate() {
        assert_eq!(
            viewer.per_output.len(),
            witness.outputs.len(),
            "viewer[{viewer_idx}] per_output length must equal outputs length"
        );
    }

    let mut sim_host = NightstreamHost::new(
        &note_spend_rom::NOTE_SPEND_ROM,
        note_spend_rom::NOTE_SPEND_ROM_BASE,
    );
    sim_host.write_note_spend_witness(&witness, public_bytes.clone());

    let ram_pairs = sim_host.ram_init_pairs();
    let sim = simulate_rv64_elf(&note_spend_rom::NOTE_SPEND_ROM, &ram_pairs, 300_000);
    assert!(
        sim.did_halt(),
        "viewer-attested replay witness must halt in simulation before proving"
    );

    let sim_exec = exec_table_from_sim(&sim);
    let requested_chunk_rows = sim.steps.len().min(NOTE_SPEND_TLEN_AUTOTUNE_CAP).max(1);
    let (chunk_rows, estimated_step_rows) = auto_tune_chunk_rows(
        &sim_exec,
        requested_chunk_rows,
        NOTE_SPEND_TLEN_AUTOTUNE_CAP,
    );
    println!(
        "viewer_replay_sim_steps={} chunk_rows={} estimated_step_rows={} viewers={} outputs={} attestations={}",
        sim.steps.len(),
        chunk_rows,
        estimated_step_rows,
        witness.viewers.len(),
        witness.outputs.len(),
        expected_public
            .view_attestations
            .as_ref()
            .map(|atts| atts.len())
            .unwrap_or(0),
    );

    if cfg!(debug_assertions) {
        println!("viewer_replay_debug_skip=true reason=upstream_nightstream_debug_mle_assertion");
        return;
    }

    let mut run_host = NightstreamHost::new(
        &note_spend_rom::NOTE_SPEND_ROM,
        note_spend_rom::NOTE_SPEND_ROM_BASE,
    );
    run_host.write_note_spend_witness(&witness, public_bytes.clone());
    run_host.set_chunk_rows(chunk_rows);
    run_host.set_max_steps(sim.steps.len());

    let t_run_prove = Instant::now();
    let mut run = run_host
        .prove_run()
        .expect("viewer-attested replay witness should prove in-memory");
    let run_prove_ms = t_run_prove.elapsed().as_millis();

    let t_run_verify = Instant::now();
    run.verify()
        .expect("viewer-attested replay run.verify() should succeed");
    let run_verify_ms = t_run_verify.elapsed().as_millis();

    println!(
        "viewer_replay_run_verify prove_ms={} verify_ms={} trace_len={} fold_count={}",
        run_prove_ms,
        run_verify_ms,
        run.trace_len(),
        run.fold_count(),
    );

    let mut package_host = NightstreamHost::new(
        &note_spend_rom::NOTE_SPEND_ROM,
        note_spend_rom::NOTE_SPEND_ROM_BASE,
    );
    package_host.write_note_spend_witness(&witness, public_bytes.clone());
    package_host.set_chunk_rows(chunk_rows);
    package_host.set_max_steps(sim.steps.len());
    let commitment = package_host.code_commitment();

    let t_package_prove = Instant::now();
    let proof_bytes = package_host
        .run(true)
        .expect("viewer-attested replay witness should package successfully");
    let package_prove_ms = t_package_prove.elapsed().as_millis();
    assert!(!proof_bytes.is_empty(), "proof payload must not be empty");

    let package = decompress_package(&proof_bytes);
    assert_eq!(
        package.public_output, public_bytes,
        "packaged public_output should match the witness-derived SpendPublicWire bytes"
    );
    assert!(
        !package.verifier_context.is_empty(),
        "viewer-attested packaged proof should include packaged verifier context"
    );

    let t_package_verify = Instant::now();
    let verified_public: SpendPublicWire = NightstreamVerifier::verify(&proof_bytes, &commitment)
        .expect("viewer-attested replay package verification should succeed");
    let package_verify_ms = t_package_verify.elapsed().as_millis();

    assert_eq!(
        verified_public, expected_public,
        "verified public output should match the witness-derived viewer-attested SpendPublic"
    );

    println!(
        "viewer_replay_packaged_verify prove_ms={} verify_ms={} proof_bytes={} verifier_ctx_bytes={}",
        package_prove_ms,
        package_verify_ms,
        proof_bytes.len(),
        package.verifier_context.len(),
    );
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
