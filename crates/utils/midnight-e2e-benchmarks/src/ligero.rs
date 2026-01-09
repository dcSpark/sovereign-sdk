use anyhow::{anyhow, Result};
use sov_rollup_interface::zk::{CodeCommitment, ZkvmHost};

/// Paths and identifiers required to run Ligero proofs and verification.
#[derive(Clone, Debug)]
pub struct LigeroEnv {
    pub program_path: String,
    pub method_id: [u8; 32],
}

/// Locate Ligero binaries and program artifacts, compute the method id, and
/// export the environment variables that downstream tools expect.
pub fn setup_ligero_env() -> Result<LigeroEnv> {
    // Pass a circuit name (not a filesystem path). `ligero-runner` resolves the correct wasm.
    let program =
        std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());

    let host =
        <sov_ligero_adapter::Ligero as sov_rollup_interface::zk::Zkvm>::Host::from_args(&program);
    let code_commitment = host.code_commitment();
    let method_id: [u8; 32] = code_commitment
        .encode()
        .try_into()
        .map_err(|_| anyhow!("Code commitment should be 32 bytes"))?;

    // Export the env vars that are still required by downstream tools.
    //
    // NOTE: Prover/verifier binary discovery now lives in `ligero-runner` and uses
    // the portable binaries shipped with the Ligero repo. Sovereign callers should not need
    // to set `LIGERO_PROVER_BIN`, `LIGERO_VERIFIER_BIN`, or `LIGERO_SHADER_PATH`.
    std::env::set_var("LIGERO_PROGRAM_PATH", &program);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok(LigeroEnv {
        program_path: program,
        method_id,
    })
}
