/*
 * RISC-V Guest Program: Value Validator
 *
 * Validates that a proven value is within range [0, 65535] (u16 range) and
 * matches the claimed value. This is the RISC-V equivalent of the WASM
 * value_validator circuit used by the Ligero prover.
 *
 * Input (via Neo ABI):
 *   proven  — u32: the value being proven in ZK
 *   claimed — u32: the value from the transaction input
 *
 * Output:
 *   u32: the validated value (on success); halts on failure.
 */

#![no_std]
#![no_main]

#[derive(nightstream_sdk::NeoAbi)]
struct ValidatorInput {
    proven: u32,
    claimed: u32,
}

#[nightstream_sdk::provable]
fn validate(input: ValidatorInput) -> u32 {
    // Value must be in u16 range [0, 65535].
    if input.proven > 65535 {
        nightstream_sdk::halt();
    }

    // Proven value must match the claimed value.
    if input.proven != input.claimed {
        nightstream_sdk::halt();
    }

    // Return the validated value.
    input.proven
}
