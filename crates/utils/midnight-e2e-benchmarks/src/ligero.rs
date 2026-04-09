use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

/// Paths and identifiers required for Nightstream proofs and verification.
#[derive(Clone, Debug)]
pub struct LigeroEnv {
    pub program_path: String,
    pub method_id: [u8; 32],
}

/// Compute the Nightstream method_id from the note_spend ROM and export env vars.
pub fn setup_ligero_env() -> Result<LigeroEnv> {
    use sov_nightstream_adapter::circuits::note_spend_rom;

    let program =
        std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());

    // Method ID = SHA-256 of ROM bytes (Nightstream code commitment).
    let method_id: [u8; 32] = Sha256::digest(&note_spend_rom::NOTE_SPEND_ROM)[..]
        .try_into()
        .map_err(|_| anyhow!("SHA-256 digest should be 32 bytes"))?;

    std::env::set_var("LIGERO_PROGRAM_PATH", &program);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok(LigeroEnv {
        program_path: program,
        method_id,
    })
}
