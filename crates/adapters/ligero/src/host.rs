//! Host implementation for Ligero zkVM

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sov_rollup_interface::zk::ZkvmHost;

use crate::{LigeroCodeCommitment, LigeroGuest, LigeroProofPackage};

/// Argument type for Ligero prover
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LigeroArg {
    /// String argument
    #[serde(rename = "str")]
    String {
        /// String value
        str: String,
    },
    /// i64 argument
    #[serde(rename = "i64")]
    I64 {
        /// i64 value
        i64: i64,
    },
    /// Hex argument
    #[serde(rename = "hex")]
    Hex {
        /// Hex string value
        hex: String,
    },
}

/// Configuration for Ligero prover/verifier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LigeroConfig {
    /// Path to the WASM program
    pub program: String,
    /// Path to shader directory
    #[serde(rename = "shader-path")]
    pub shader_path: String,
    /// Packing size (FFT message packing size)
    pub packing: u32,
    /// Indices of private arguments (1-based)
    #[serde(rename = "private-indices")]
    pub private_indices: Vec<usize>,
    /// Program arguments
    pub args: Vec<LigeroArg>,
}

/// Host for Ligero zkVM
#[derive(Clone)]
pub struct LigeroHost {
    config: LigeroConfig,
    prover_bin: PathBuf,
    verifier_bin: PathBuf,
    bins_dir: PathBuf,
    public_output: Option<Vec<u8>>,
}

impl LigeroHost {
    /// Create a new LigeroHost with the given WASM program path
    pub fn new(program_path: &str) -> Self {
        let bins_dir = Self::find_bins_dir();
        // Use absolute path for shader_path to work from any working directory
        let shader_path = bins_dir
            .canonicalize()
            .unwrap_or_else(|_| bins_dir.clone())
            .join("shader")
            .to_string_lossy()
            .to_string();

        Self {
            config: LigeroConfig {
                program: program_path.to_string(),
                shader_path,
                packing: 8192,
                private_indices: vec![],
                args: vec![],
            },
            prover_bin: bins_dir.join("webgpu_prover"),
            verifier_bin: bins_dir.join("webgpu_verifier"),
            bins_dir,
            public_output: None,
        }
    }

    /// Find the bins directory
    fn find_bins_dir() -> PathBuf {
        // Try to find relative to the crate root
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let bins_dir = PathBuf::from(manifest_dir).join("bins");

        if bins_dir.exists() {
            return bins_dir;
        }

        // Fallback to current directory
        PathBuf::from("bins")
    }

    /// Set the packing size
    pub fn with_packing(mut self, packing: u32) -> Self {
        self.config.packing = packing;
        self
    }

    /// Set private argument indices (1-based)
    pub fn with_private_indices(mut self, indices: Vec<usize>) -> Self {
        self.config.private_indices = indices;
        self
    }

    /// Record the public output that will be embedded in the proof package.
    ///
    /// The value is serialized using `bincode` so the verifier can recover it.
    pub fn set_public_output<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let bytes = bincode::serialize(value)
            .context("Failed to serialize Ligero public output with bincode")?;
        self.public_output = Some(bytes);
        Ok(())
    }

    /// Record the public output using raw bytes.
    pub fn set_public_output_bytes(&mut self, bytes: Vec<u8>) {
        self.public_output = Some(bytes);
    }

    /// Add a string argument
    pub fn add_str_arg(&mut self, value: String) {
        self.config.args.push(LigeroArg::String { str: value });
    }

    /// Add an i64 argument
    pub fn add_i64_arg(&mut self, value: i64) {
        self.config.args.push(LigeroArg::I64 { i64: value });
    }

    /// Add a hex argument
    pub fn add_hex_arg(&mut self, value: String) {
        self.config.args.push(LigeroArg::Hex { hex: value });
    }

    /// Run the prover and generate a proof
    fn run_prover(&self) -> Result<Vec<u8>> {
        let config_json =
            serde_json::to_string(&self.config).context("Failed to serialize Ligero config")?;

        tracing::debug!("Running Ligero prover with config: {}", config_json);

        // Run prover in current directory so proof.data is written to CWD
        // This allows parallel proof generation in worker-specific directories
        let output = Command::new(&self.prover_bin)
            .arg(&config_json)
            .output()
            .context("Failed to execute webgpu_prover")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!(
                "Ligero prover failed with status {:?}\nstdout: {}\nstderr: {}",
                output.status.code(),
                stdout,
                stderr
            );
        }

        // Check if the output indicates success
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.contains("Final prove result:                  true") {
            anyhow::bail!("Ligero prover did not produce a valid proof");
        }

        // Read the proof from proof.data (in current working directory)
        let proof_path = PathBuf::from("proof.data");
        let proof = std::fs::read(&proof_path).context("Failed to read proof.data")?;

        tracing::debug!("Proof generated successfully, size: {} bytes", proof.len());
        Ok(proof)
    }

    /// Verify a proof (used for testing)
    #[cfg(feature = "native")]
    pub fn verify_proof(&self) -> Result<bool> {
        let config_json =
            serde_json::to_string(&self.config).context("Failed to serialize Ligero config")?;

        tracing::debug!("Running Ligero verifier with config: {}", config_json);

        let output = Command::new(&self.verifier_bin)
            .arg(&config_json)
            .current_dir(&self.bins_dir)
            .output()
            .context("Failed to execute webgpu_verifier")?;

        if !output.status.success() {
            return Ok(false);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.contains("Final Verify Result:                 true"))
    }
}

impl ZkvmHost for LigeroHost {
    type Guest = LigeroGuest;
    type HostArgs = String;

    fn from_args(program_path: &Self::HostArgs) -> Self {
        Self::new(program_path)
    }

    fn add_hint<T: Serialize>(&mut self, _item: T) {
        // Ligero doesn't use hints in the same way
        // Arguments should be added using add_*_arg methods
    }

    fn code_commitment(
        &self,
    ) -> <<Self::Guest as sov_rollup_interface::zk::ZkvmGuest>::Verifier as sov_rollup_interface::zk::ZkVerifier>::CodeCommitment{
        // For Ligero, the code commitment is the hash of the WASM program
        use sha2::{Digest, Sha256};

        let program_bytes = std::fs::read(&self.config.program).unwrap_or_default();

        let mut hasher = Sha256::new();
        hasher.update(&program_bytes);
        hasher.update(&self.config.packing.to_le_bytes());

        let hash = hasher.finalize();
        let mut commitment = [0u8; 32];
        commitment.copy_from_slice(&hash);

        LigeroCodeCommitment(commitment)
    }

    fn run(&mut self, with_proof: bool) -> Result<Vec<u8>> {
        if with_proof {
            let public_output = self
                .public_output
                .clone()
                .ok_or_else(|| anyhow!("Ligero public output not set; call set_public_output before generating a proof"))?;

            tracing::info!("Ligero: Generating proof with webgpu_prover");
            let proof = self.run_prover()?;
            let package = LigeroProofPackage {
                proof,
                public_output,
            };
            Ok(bincode::serialize(&package)?)
        } else {
            tracing::info!("Ligero: Executing without proof generation (simulation mode)");

            #[cfg(feature = "native")]
            {
                // Run a lightweight verification check if verification is enabled
                // This helps catch errors early without the cost of full proving
                if let Err(e) = self.verify_proof() {
                    tracing::warn!("Ligero execution check failed: {}", e);
                }
            }

            let public_output = self.public_output.clone().unwrap_or_else(|| {
                tracing::debug!(
                    "LigeroHost::run executed without configured public output; returning empty payload"
                );
                Vec::new()
            });

            let package = LigeroProofPackage {
                proof: vec![],
                public_output,
            };
            Ok(bincode::serialize(&package)?)
        }
    }
}
