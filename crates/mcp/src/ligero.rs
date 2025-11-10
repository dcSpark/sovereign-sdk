//! Zero-knowledge proof generation module for value-setter-zk transactions
//!
//! This module provides functionality to generate Ligero proofs that demonstrate
//! a value is within a valid range without revealing the computation details.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The public output structure that matches the guest program and value-setter-zk module
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LigeroProgramArguments {
    STR { str: String },
    I64 { i64: i64 },
    HEX { hex: String },
}

/// Configuration for proof generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LigeroArgument {
    pub program: String,

    #[serde(rename = "gpu-threads")]
    pub gpu_threads: u32,

    #[serde(rename = "shader-path")]
    pub shader_path: String,

    pub packing: u32,

    #[serde(rename = "private-indices")]
    pub private_indices: Vec<u32>,

    pub args: Vec<LigeroProgramArguments>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ligero {
    ligero_prover_binary_path: Option<PathBuf>,
    ligero_verifier_binary_path: Option<PathBuf>,
    ligero_shader_path: Option<PathBuf>,
    ligero_program_path: Option<PathBuf>,
    proof_dir_id: Option<String>,
}

impl Ligero {
    const LIGERO_PROOF_FILE_NAME: &str = "proof_data.gz";

    pub fn new(
        ligero_prover_binary_path: Option<PathBuf>,
        ligero_verifier_binary_path: Option<PathBuf>,
        ligero_shader_path: Option<PathBuf>,
        ligero_program_path: Option<PathBuf>,
    ) -> Self {
        Self {
            ligero_prover_binary_path,
            ligero_verifier_binary_path,
            ligero_shader_path,
            ligero_program_path,
            proof_dir_id: None,
        }
    }

    /// Set a custom identifier for the proof directory (for deterministic paths)
    /// This is useful for debugging and ensures proof directories have meaningful names
    #[allow(dead_code)]
    pub fn set_proof_dir_id(&mut self, id: String) {
        self.proof_dir_id = Some(id);
    }

    pub fn generate_proof(
        &self,
        packing: u32,
        gpu_threads: u32,
        private_indices: Vec<u32>,
        args: Vec<LigeroProgramArguments>,
    ) -> Result<Vec<u8>> {
        if self.ligero_prover_binary_path.is_none()
            || self.ligero_shader_path.is_none()
            || self.ligero_program_path.is_none()
        {
            anyhow::bail!("ligero prover binary path, shader path, and program path are required");
        }

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

        let proof_path = unique_proof_dir.join(Self::LIGERO_PROOF_FILE_NAME);
        tracing::info!("generating ligero proof at {}", proof_path.display());

        let ligero_program_path = self.ligero_program_path.clone().unwrap().canonicalize().unwrap();
        let ligero_shader_path = self.ligero_shader_path.clone().unwrap().canonicalize().unwrap();

        let ligero_argument = LigeroArgument {
            program: ligero_program_path.to_string_lossy().into_owned(),
            shader_path: ligero_shader_path.to_string_lossy().into_owned(),
            packing: packing.clone(),
            gpu_threads: gpu_threads.clone(),
            private_indices: private_indices.clone(),
            args: args.clone(),
        };

        let ligero_argument_json = serde_json::to_string(&ligero_argument)
            .context("failed to serialize ligero argument")?;

        tracing::info!(
            "ligero prover binary path: {}",
            self.ligero_prover_binary_path.clone().unwrap().display()
        );
        tracing::info!("ligero argument: {}", ligero_argument_json);

        let ligero_prover_binary_path = self.ligero_prover_binary_path.clone().unwrap().canonicalize().unwrap();

        let output = Command::new(&ligero_prover_binary_path)
            .current_dir(&unique_proof_dir)
            .arg(&ligero_argument_json)
            .output()
            .inspect_err(|e| tracing::info!("failed to execute ligero prover: {:?}", e))
            .context("failed to execute ligero prover")?;

        tracing::info!(
            "ligero prover execution finished with status {:?}",
            output.status.code()
        );
        tracing::info!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        tracing::info!("stderr: {}", String::from_utf8_lossy(&output.stderr));

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Clean up the temporary directory on failure
            let _ = std::fs::remove_dir_all(&unique_proof_dir);
            anyhow::bail!(
                "ligero prover failed with status {:?}\nstdout: {}\nstderr: {}",
                output.status.code(),
                stdout,
                stderr
            );
        }

        // Check if the output indicates success
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.contains("Final prove result:                  true") {
            tracing::info!("ligero prover did not produce a valid proof");
            // Clean up the temporary directory on failure
            let _ = std::fs::remove_dir_all(&unique_proof_dir);
            anyhow::bail!("ligero prover did not produce a valid proof");
        }

        tracing::info!("ligero prover generated successfully");

        // Read the proof from proof_data.gz (compressed - this goes into the transaction)
        let proof = std::fs::read(&proof_path).context("failed to read proof_data.gz")?;
        
        // Clean up the temporary directory after reading the proof
        if let Err(e) = std::fs::remove_dir_all(&unique_proof_dir) {
            tracing::warn!("Failed to clean up temporary proof directory: {}", e);
        }
        
        Ok(proof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::ligero::create_test_ligero;

    #[tracing_test::traced_test]
    #[test]
    fn test_generate_proof() {
        let ligero = create_test_ligero();

        let proof = ligero
            .generate_proof(
                8192,
                8000,
                vec![1],
                vec![
                    LigeroProgramArguments::I64 { i64: 1 },
                    LigeroProgramArguments::I64 { i64: 1 },
                ],
            )
            .unwrap();
        assert!(!proof.is_empty());
    }
}
