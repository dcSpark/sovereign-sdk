//! Host implementation for Ligero zkVM

use anyhow::{anyhow, Context, Result};
pub use ligero_webgpu_runner::{LigeroArg, LigeroConfig};
use ligero_webgpu_runner::LigeroRunner;
use serde::Serialize;
use sov_rollup_interface::zk::ZkvmHost;

use crate::{LigeroCodeCommitment, LigeroGuest, LigeroProofPackage};

/// Host for Ligero zkVM
#[derive(Clone)]
pub struct LigeroHost {
    runner: LigeroRunner,
    public_output: Option<Vec<u8>>,
}

impl LigeroHost {
    /// Create a new LigeroHost with the given WASM program path
    pub fn new(program_path: &str) -> Self {
        Self {
            runner: LigeroRunner::new(program_path),
            public_output: None,
        }
    }

    /// Set the packing size
    pub fn with_packing(mut self, packing: u32) -> Self {
        self.runner = self.runner.with_packing(packing);
        self
    }

    /// Set private argument indices (1-based)
    pub fn with_private_indices(mut self, indices: Vec<usize>) -> Self {
        self.runner = self.runner.with_private_indices(indices);
        self
    }

    /// Set a custom identifier for the proof directory (for deterministic paths)
    /// This is useful for debugging and ensures proof directories have meaningful names
    pub fn with_proof_dir_id(mut self, id: String) -> Self {
        self.runner = self.runner.with_proof_dir_id(id);
        self
    }

    /// Set a custom identifier for the proof directory (mutable version)
    pub fn set_proof_dir_id(&mut self, id: String) {
        self.runner.set_proof_dir_id(id);
    }

    /// Record the public output that will be embedded in the proof package.
    ///
    /// The value is serialized using `bincode` so the verifier can recover it.
    pub fn set_public_output<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let bytes =
            bincode::serialize(value).context("Failed to serialize Ligero public output with bincode")?;
        self.public_output = Some(bytes);
        Ok(())
    }

    /// Record the public output using raw bytes.
    pub fn set_public_output_bytes(&mut self, bytes: Vec<u8>) {
        self.public_output = Some(bytes);
    }

    /// Add a string argument
    pub fn add_str_arg(&mut self, value: String) {
        self.runner.add_str_arg(value);
    }

    /// Add an i64 argument
    pub fn add_i64_arg(&mut self, value: i64) {
        self.runner.add_i64_arg(value);
    }

    /// Add a u64 argument (encoded as i64; guest checks non-negative)
    pub fn add_u64_arg(&mut self, value: u64) {
        assert!(
            value <= i64::MAX as u64,
            "u64 value too large for i64 encoding"
        );
        self.runner.add_i64_arg(value as i64);
    }

    /// Add a hex argument.
    ///
    /// Accepts either raw hex or `0x`-prefixed hex; stores without the prefix.
    pub fn add_hex_arg(&mut self, value: String) {
        let hex = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .map(|s| s.to_string())
            .unwrap_or(value);
        self.runner.add_hex_arg(hex);
    }

    /// Get the program path used by this host.
    pub fn program_path(&self) -> &str {
        &self.runner.config().program
    }

    /// Get the shader path used by this host.
    pub fn shader_path(&self) -> &str {
        &self.runner.config().shader_path
    }

    /// Get the packing parameter used by this host.
    pub fn packing(&self) -> u32 {
        self.runner.config().packing
    }

    /// Get the verifier binary path (best-effort discovered by the runner).
    pub fn verifier_bin(&self) -> &std::path::PathBuf {
        &self.runner.paths().verifier_bin
    }

    /// Verify a proof for the current config (smoke check).
    ///
    /// Returns `true` if the verifier prints a successful result.
    pub fn verify_proof(&self) -> Result<bool> {
        self.runner.verify_proof_smoke()
    }

    /// Generate a proof and also return the prover's stdout for debugging.
    ///
    /// Returns `(serialized_proof_package, prover_stdout)`.
    pub fn run_with_logging(&mut self) -> Result<(Vec<u8>, String)> {
        let public_output = self.public_output.clone().ok_or_else(|| {
            anyhow!("Ligero public output not set; call set_public_output before generating a proof")
        })?;

        let (proof, stdout, _stderr) = self
            .runner
            .run_prover_with_output(ligero_webgpu_runner::ProverRunOptions::default())?;

        let package = LigeroProofPackage {
            proof,
            public_output,
            args_json: serde_json::to_vec(&self.runner.config().args)?,
            private_indices: self.runner.config().private_indices.clone(),
        };

        Ok((bincode::serialize(&package)?, stdout))
    }
}

impl ZkvmHost for LigeroHost {
    type Guest = LigeroGuest;
    type HostArgs = String;

    fn from_args(program_path: &Self::HostArgs) -> Self {
        Self::new(program_path)
    }

    fn add_hint<T: Serialize>(&mut self, _item: T) {
        // Ligero doesn't use hints in the same way.
        // Arguments should be added using add_*_arg methods.
    }

    fn code_commitment(
        &self,
    ) -> <<Self::Guest as sov_rollup_interface::zk::ZkvmGuest>::Verifier as sov_rollup_interface::zk::ZkVerifier>::CodeCommitment{
        // For Ligero, the code commitment is the hash of the WASM program + packing.
        use sha2::{Digest, Sha256};

        let program_bytes = std::fs::read(&self.runner.config().program).unwrap_or_default();

        let mut hasher = Sha256::new();
        hasher.update(&program_bytes);
        hasher.update(&self.runner.config().packing.to_le_bytes());

        let hash = hasher.finalize();
        let mut commitment = [0u8; 32];
        commitment.copy_from_slice(&hash);

        LigeroCodeCommitment(commitment)
    }

    fn run(&mut self, with_proof: bool) -> Result<Vec<u8>> {
        if with_proof {
            let public_output = self.public_output.clone().ok_or_else(|| {
                anyhow!("Ligero public output not set; call set_public_output before generating a proof")
            })?;

            tracing::info!("Ligero: Generating proof with webgpu_prover");
            let proof = self.runner.run_prover()?;

            let package = LigeroProofPackage {
                proof,
                public_output,
                args_json: serde_json::to_vec(&self.runner.config().args)?,
                private_indices: self.runner.config().private_indices.clone(),
            };

            Ok(bincode::serialize(&package)?)
        } else {
            tracing::info!("Ligero: Executing without proof generation (simulation mode)");

            if let Err(e) = self.runner.verify_proof_smoke() {
                tracing::warn!("Ligero execution check failed: {}", e);
            }

            let public_output = self.public_output.clone().unwrap_or_else(|| Vec::new());

            let package = LigeroProofPackage {
                proof: vec![],
                public_output,
                args_json: serde_json::to_vec(&self.runner.config().args)?,
                private_indices: self.runner.config().private_indices.clone(),
            };

            Ok(bincode::serialize(&package)?)
        }
    }
}
