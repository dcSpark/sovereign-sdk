//! Zero-knowledge proof generation module for value-setter-zk transactions
//!
//! This module provides functionality to generate Ligero proofs that demonstrate
//! a value is within a valid range without revealing the computation details.

use std::env::temp_dir;
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
        }
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

        let proof_path = std::env::temp_dir().join(Self::LIGERO_PROOF_FILE_NAME);

        let ligero_program_path = self.ligero_program_path.clone().unwrap();
        let ligero_shader_path = self.ligero_shader_path.clone().unwrap();

        tracing::info!("generating Ligero proof at {}", proof_path.display());

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

        let ligero_prover_binary_path = self.ligero_prover_binary_path.clone().unwrap();
        let ligero_prover_execution_path = proof_path.parent().unwrap();

        let output = Command::new(&ligero_prover_binary_path)
            .current_dir(ligero_prover_execution_path)
            .arg(&ligero_argument_json)
            .output()
            .inspect_err(|e| tracing::error!("failed to execute ligero prover: {:?}", e))
            .context("failed to execute ligero prover")?;

        tracing::info!("ligero prover output: {:?}", output);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
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
            anyhow::bail!("Ligero prover did not produce a valid proof");
        }

        // Read the proof from proof_data.gz (compressed - this goes into the transaction)
        let proof_path = PathBuf::from(Self::LIGERO_PROOF_FILE_NAME);
        let proof = std::fs::read(&proof_path).context("Failed to read proof_data.gz")?;
        Ok(proof)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[tracing_test::traced_test]
    #[test]
    fn test_generate_proof() {
        let ligero = Ligero::new(
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../adapters/ligero/bins/macos/bin/webgpu_prover")
                    .canonicalize()
                    .unwrap(),
            ),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../adapters/ligero/bins/macos/bin/webgpu_verifier")
                    .canonicalize()
                    .unwrap(),
            ),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../adapters/ligero/bins/shader")
                    .canonicalize()
                    .unwrap(),
            ),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../adapters/ligero/bins/programs/value_validator.wasm")
                    .canonicalize()
                    .unwrap(),
            ),
        );

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
