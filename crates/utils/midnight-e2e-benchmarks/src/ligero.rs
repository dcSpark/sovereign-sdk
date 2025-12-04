use anyhow::{anyhow, Result};
use sov_rollup_interface::zk::{CodeCommitment, ZkvmHost};

/// Paths and identifiers required to run Ligero proofs and verification.
#[derive(Clone, Debug)]
pub struct LigeroEnv {
    pub program_path: String,
    pub method_id: [u8; 32],
    pub prover_bin: String,
    pub verifier_bin: String,
    pub shader_dir: String,
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
    let platform_dir = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        anyhow::bail!("Unsupported platform");
    };
    let bin_dir = ligero_dir.join("bins").join(platform_dir).join("bin");
    let shader_dir = ligero_dir.join("bins").join(platform_dir).join("shader");
    let program_path = ligero_dir.join("guest/bins/programs/note_spend_guest.wasm");
    let prover_bin = bin_dir.join("webgpu_prover");
    let verifier_bin = bin_dir.join("webgpu_verifier");

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

    // Export the env vars so callers don't need to set them themselves.
    std::env::set_var("LIGERO_PROGRAM_PATH", &program_path);
    std::env::set_var("LIGERO_PROVER_BIN", &prover_bin);
    std::env::set_var("LIGERO_VERIFIER_BIN", &verifier_bin);
    std::env::set_var("LIGERO_SHADER_PATH", &shader_dir);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok(LigeroEnv {
        program_path: program_path.to_string_lossy().to_string(),
        method_id,
        prover_bin: prover_bin.to_string_lossy().to_string(),
        verifier_bin: verifier_bin.to_string_lossy().to_string(),
        shader_dir: shader_dir.to_string_lossy().to_string(),
    })
}
