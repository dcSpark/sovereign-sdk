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
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .ok_or_else(|| anyhow!("Could not find repository root"))?;
    let ligero_dir = repo_root.join("crates/adapters/ligero");
    let program_path = ligero_dir.join("guest/bins/programs/note_spend_guest.wasm");

    anyhow::ensure!(
        program_path.exists(),
        "note_spend_guest.wasm not found at {}",
        program_path.display()
    );

    let host = <sov_ligero_adapter::Ligero as sov_rollup_interface::zk::Zkvm>::Host::from_args(
        &program_path.to_string_lossy().to_string(),
    );
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
    std::env::set_var("LIGERO_PROGRAM_PATH", &program_path);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok(LigeroEnv {
        program_path: program_path.to_string_lossy().to_string(),
        method_id,
    })
}
