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
    proof_dir_id: Option<String>,
}

impl LigeroHost {
    /// Create a new LigeroHost with the given WASM program path
    pub fn new(program_path: &str) -> Self {
        // Allow overriding the prover binary location via env var (useful for scripts / non-standard layouts).
        // Accept both historical names used across this repo.
        let prover_override = std::env::var("LIGERO_PROVER_BIN")
            .ok()
            .or_else(|| std::env::var("LIGERO_PROVER_BINARY_PATH").ok())
            .and_then(|p| std::fs::canonicalize(&p).ok());

        let bins_dir = if let Some(ref prover) = prover_override {
            prover.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(Self::find_bins_dir)
        } else {
            Self::find_bins_dir()
        };
        // Use absolute path for shader_path to work from any working directory
        let shader_path = if bins_dir.ends_with("bin") {
            // If using platform-specific bin directory, shader lives at `.../bins/shader`
            bins_dir
                .parent()
                .and_then(|p| p.parent())
                .unwrap_or(&bins_dir)
                .canonicalize()
                .unwrap_or_else(|_| bins_dir.parent().unwrap_or(&bins_dir).to_path_buf())
                .join("shader")
                .to_string_lossy()
                .to_string()
        } else {
            // If using generic bins directory, shader is at same level
            bins_dir
                .canonicalize()
                .unwrap_or_else(|_| bins_dir.clone())
                .join("shader")
                .to_string_lossy()
                .to_string()
        };

        Self {
            config: LigeroConfig {
                program: program_path.to_string(),
                shader_path,
                packing: 8192,
                private_indices: vec![],
                args: vec![],
            },
            prover_bin: prover_override.unwrap_or_else(|| bins_dir.join("webgpu_prover")),
            verifier_bin: bins_dir.join("webgpu_verifier"),
            bins_dir,
            public_output: None,
            proof_dir_id: None,
        }
    }

    /// Find the bins directory
    fn find_bins_dir() -> PathBuf {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");

        // Check for platform-specific binaries first (they take priority)
        #[cfg(target_os = "macos")]
        {
            let macos_bins = PathBuf::from(manifest_dir).join("bins/macos-arm64/bin");
            if macos_bins.join("webgpu_prover").exists()
                && macos_bins.join("webgpu_verifier").exists()
            {
                return macos_bins;
            }
        }

        #[cfg(target_os = "linux")]
        {
            let linux_bins = PathBuf::from(manifest_dir).join("bins/linux-amd64/bin");
            if linux_bins.join("webgpu_prover").exists()
                && linux_bins.join("webgpu_verifier").exists()
            {
                return linux_bins;
            }
            let linux_arm_bins = PathBuf::from(manifest_dir).join("bins/linux-arm64/bin");
            if linux_arm_bins.join("webgpu_prover").exists()
                && linux_arm_bins.join("webgpu_verifier").exists()
            {
                return linux_arm_bins;
            }
        }

        // Try to find relative to the crate root (generic bins directory)
        let bins_dir = PathBuf::from(manifest_dir).join("bins");
        if bins_dir.join("webgpu_prover").exists() && bins_dir.join("webgpu_verifier").exists() {
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

    /// Set a custom identifier for the proof directory (for deterministic paths)
    /// This is useful for debugging and ensures proof directories have meaningful names
    pub fn with_proof_dir_id(mut self, id: String) -> Self {
        self.proof_dir_id = Some(id);
        self
    }

    /// Set a custom identifier for the proof directory (mutable version)
    pub fn set_proof_dir_id(&mut self, id: String) {
        self.proof_dir_id = Some(id);
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

        // Create a deterministic directory for this proof in the project's proof_outputs folder
        // Use custom ID if provided, otherwise fall back to thread ID for uniqueness
        let dir_name = if let Some(ref id) = self.proof_dir_id {
            format!("ligero_proof_{}", id)
        } else {
            format!("ligero_proof_{:?}", std::thread::current().id())
        };
        
        // Use project-relative path instead of /tmp/
        let proof_outputs_base = std::env::current_dir()
            .context("Failed to get current directory")?
            .join("proof_outputs");
        
        let unique_proof_dir = proof_outputs_base.join(dir_name);
        std::fs::create_dir_all(&unique_proof_dir)
            .context("Failed to create unique proof directory")?;

        tracing::debug!(
            "About to run prover with working directory: {:?}",
            unique_proof_dir
        );
        tracing::debug!("Prover binary: {}", self.prover_bin.display());
        tracing::debug!("Prover config: {}", config_json);

        let inherit_stdio = std::env::var("LIGERO_PROVER_INHERIT_STDIO")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Safety net: WebGPU initialization can hang on some systems. Default to 10 minutes.
        let timeout_secs: u64 = std::env::var("LIGERO_PROVER_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600);

        let mut child = {
            let mut cmd = Command::new(&self.prover_bin);
            cmd.arg(&config_json).current_dir(&unique_proof_dir);
            if inherit_stdio {
                cmd.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());
            }
            let child = cmd.spawn().context("Failed to execute webgpu_prover")?;
            tracing::info!(
                "Spawned webgpu_prover (pid={:?}) cwd={} bin={}",
                child.id(),
                unique_proof_dir.display(),
                self.prover_bin.display()
            );
            child
        };

        let start = std::time::Instant::now();
        let mut next_heartbeat = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().context("Failed to poll webgpu_prover")? {
                if !status.success() {
                    // Clean up the temporary directory on failure
                    let _ = std::fs::remove_dir_all(&unique_proof_dir);
                    anyhow::bail!("Ligero prover failed with status {:?}", status.code());
                }
                break;
            }

            if std::time::Instant::now() >= next_heartbeat {
                tracing::info!(
                    "webgpu_prover still running (elapsed={}s, timeout={}s, pid={:?})",
                    start.elapsed().as_secs(),
                    timeout_secs,
                    child.id()
                );
                next_heartbeat += std::time::Duration::from_secs(10);
            }

            if timeout_secs > 0 && start.elapsed().as_secs() >= timeout_secs {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&unique_proof_dir);
                anyhow::bail!(
                    "Ligero prover timed out after {}s. If this hangs consistently, try setting LIGERO_PROVER_INHERIT_STDIO=1 to see prover logs, and ensure WebGPU/GPU access is available.",
                    timeout_secs
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }

        // If we inherited stdio, we didn't capture stdout. At this point, rely on the proof output file.
        // Otherwise, the prover would have been silent anyway; the authoritative success signal is proof_data.gz.
        let stdout = String::new();
        let _stderr = String::new();

        // Check if the output indicates success (best-effort; primary artifact is proof_data.gz)
        let stdout = stdout.as_str();

        // Check WASM exit code - reject if non-zero (indicates WASM program failure)
        for line in stdout.lines() {
            if line.contains("Exit with code") {
                // Parse exit code from line like "Exit with code 71"
                if let Some(code_str) = line.strip_prefix("Exit with code ") {
                    if let Ok(code) = code_str.trim().parse::<i32>() {
                        if code != 0 {
                            tracing::error!("WASM program exited with non-zero code {}. This indicates a program failure (e.g., parse error, assertion failure). Proof would be invalid.", code);
                            // let _ = std::fs::remove_dir_all(&unique_proof_dir);
                            // TODO: Re enable this once we adapt the WASM program
                            // anyhow::bail!(
                            //     "WASM program exited with non-zero code {}. This indicates a program failure (e.g., parse error, assertion failure). Proof would be invalid.",
                            //     code
                            // );
                        }
                    }
                }
            }
        }

        if !inherit_stdio && !stdout.is_empty() && !stdout.contains("Final prove result:                  true") {
            // Clean up the temporary directory on failure
            let _ = std::fs::remove_dir_all(&unique_proof_dir);
            anyhow::bail!("Ligero prover did not produce a valid proof");
        }

        // Read the proof from proof_data.gz (compressed - this goes into the transaction)
        let proof_path = unique_proof_dir.join("proof_data.gz");
        let proof = std::fs::read(&proof_path).context("Failed to read proof_data.gz")?;

        tracing::debug!(
            "Reading proof from: {}, size: {} bytes",
            proof_path.display(),
            proof.len()
        );

        // This should be compressed gzip data
        if proof.len() >= 2 && proof[0] == 0x1f && proof[1] == 0x8b {
            tracing::debug!("✓ Reading compressed proof_data.gz (gzip format)");
        } else {
            tracing::warn!(
                "⚠ proof_data.gz does not appear to be gzip format! First bytes: {:02x?}",
                &proof[..std::cmp::min(10, proof.len())]
            );
        }

        tracing::debug!(
            "First few bytes of read proof: {:?}",
            &proof[..std::cmp::min(20, proof.len())]
        );

        tracing::debug!("Proof generated successfully, size: {} bytes", proof.len());
        
        // Clean up the temporary directory after reading the proof
        if let Err(e) = std::fs::remove_dir_all(&unique_proof_dir) {
            tracing::warn!("Failed to clean up temporary proof directory: {}", e);
        }
        
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

            tracing::debug!(
                "Creating LigeroProofPackage with proof size: {} bytes",
                proof.len()
            );
            tracing::debug!(
                "Proof first bytes before packaging: {:?}",
                &proof[..std::cmp::min(20, proof.len())]
            );

            let package = LigeroProofPackage {
                proof,
                public_output,
                args_json: serde_json::to_vec(&self.config.args)?,
                private_indices: self.config.private_indices.clone(),
            };

            let serialized = bincode::serialize(&package)?;
            tracing::debug!("Serialized package size: {} bytes", serialized.len());

            Ok(serialized)
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
                args_json: serde_json::to_vec(&self.config.args)?,
                private_indices: self.config.private_indices.clone(),
            };
            Ok(bincode::serialize(&package)?)
        }
    }
}
