/*
 * RISC-V Guest Program: Note Spend Verifier (Stub)
 *
 * This is a placeholder for the full note_spend circuit. The real implementation
 * will port the WASM note_spend_guest (from ligero-prover/utils/circuits/note-spend)
 * to the Nightstream RISC-V proving VM.
 *
 * The full circuit verifies a join-split spend with:
 *   1) Merkle membership proofs for input note commitments
 *   2) PRF-based nullifier derivation (Poseidon2)
 *   3) Ownership proof (all inputs share the same spend key)
 *   4) Nullifier distinctness
 *   5) Output note commitment computation
 *   6) Balance conservation: sum(inputs) == withdraw_amount + sum(outputs)
 *   7) Blacklist non-membership proofs
 *   8) (Optional) Viewer attestations
 *
 * Requirements for the real circuit:
 *   - Poseidon2 hashing (currently via ligetron host functions in WASM version)
 *   - BN254 field arithmetic (addmod, mulmod, submod)
 *   - Merkle tree path verification with field-level MUX
 *   - Domain-separated hash constructions (MT_NODE_V1, NOTE_V2, PRF_NF_V1, etc.)
 *
 * Until the crypto primitives are available on the Nightstream RISC-V target,
 * this stub reads a single input word and passes it through.
 *
 * Input (via Neo ABI):
 *   input_word — u32: placeholder input
 *
 * Output:
 *   u32: pass-through of input_word
 */

#![no_std]
#![no_main]

#[derive(nightstream_sdk::NeoAbi)]
struct NoteSpendInput {
    input_word: u32,
}

#[nightstream_sdk::provable]
fn note_spend(input: NoteSpendInput) -> u32 {
    // TODO: Implement the full note_spend circuit.
    //
    // The WASM reference implementation is at:
    //   ligero-prover/utils/circuits/note-spend/src/main.rs
    //
    // Porting requires:
    //   1. Poseidon2 hashing on RISC-V (pure Rust or via host functions)
    //   2. BN254 Fr field arithmetic (256-bit modular ops)
    //   3. Merkle tree path verification
    //   4. Nullifier computation: H("PRF_NF_V1" || domain || nf_key || rho)
    //   5. Note commitments: H("NOTE_V2" || domain || value || rho || recipient || sender_id)
    //   6. Balance constraint: sum(input_values) == withdraw_amount + sum(output_values)
    //   7. Blacklist bucket non-membership + Merkle membership
    //   8. Viewer attestation (FVK commit, stream XOR encrypt, MAC)

    input.input_word
}
