//! Zero-knowledge proof generation module for value-setter-zk transactions
//!
//! This module provides functionality to generate Ligero proofs that demonstrate
//! a value is within a valid range without revealing the computation details.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

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

    #[serde(rename = "gpu-threads", skip_serializing_if = "Option::is_none")]
    pub gpu_threads: Option<u32>,

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

    /// Resolve prover parameters, allowing env overrides:
    /// - LIGERO_PACKING to override packing
    /// - LIGERO_GPU_THREADS to set an explicit gpu-threads value (omit to let the prover decide)
    pub fn resolve_prover_params(
        &self,
        default_packing: u32,
        default_gpu_threads: Option<u32>,
    ) -> (u32, Option<u32>) {
        let packing = std::env::var("LIGERO_PACKING")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_packing);

        let gpu_threads = std::env::var("LIGERO_GPU_THREADS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .or(default_gpu_threads);

        tracing::info!(
            packing,
            gpu_threads = gpu_threads.unwrap_or(0),
            gpu_threads_set = gpu_threads.is_some(),
            "Using Ligero prover parameters (env overrides allowed)"
        );

        (packing, gpu_threads)
    }

    /// Generate a proof with automatic handling of string arguments
    /// This is a convenience wrapper that converts string args to the appropriate format
    #[allow(dead_code)]
    pub fn generate_proof_with_public_output<T: Serialize>(
        &self,
        packing: u32,
        gpu_threads: Option<u32>,
        private_indices: Vec<u32>,
        args: Vec<String>,
        public_output: &T,
    ) -> Result<Vec<u8>> {
        // Convert string args to LigeroProgramArguments
        let ligero_args: Vec<LigeroProgramArguments> = args
            .into_iter()
            .map(|s| LigeroProgramArguments::STR { str: s })
            .collect();

        // For midnight-privacy, we need to serialize the public output and include it
        // The public output is handled by the guest program, so we just generate the proof normally
        let _ = public_output; // Public output is validated by guest, not passed explicitly

        self.generate_proof(packing, gpu_threads, private_indices, ligero_args)
    }

    pub fn generate_proof(
        &self,
        packing: u32,
        gpu_threads: Option<u32>,
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

        let ligero_program_path = self
            .ligero_program_path
            .clone()
            .unwrap()
            .canonicalize()
            .unwrap();
        let ligero_shader_path = self
            .ligero_shader_path
            .clone()
            .unwrap()
            .canonicalize()
            .unwrap();

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

        // Write a reproducible command script for manual debugging.
        // We do this before spawning the prover so the exact payload is captured.
        let escaped_json = ligero_argument_json.replace('\'', "'\\''");
        let command_script_path = proof_outputs_base.join("last_prover_command.sh");
        let script_contents = format!(
            "#!/usr/bin/env bash\nset -euo pipefail\ncd \"{}\"\n\"{}\" '{}'\n",
            unique_proof_dir.display(),
            self.ligero_prover_binary_path.clone().unwrap().display(),
            escaped_json
        );
        if let Err(e) = std::fs::write(&command_script_path, script_contents) {
            tracing::warn!(
                "Failed to write prover command script at {}: {}",
                command_script_path.display(),
                e
            );
        } else {
            tracing::info!(
                "Wrote prover replay script to {}",
                command_script_path.display()
            );
        }

        tracing::info!(
            "ligero prover binary path: {}",
            self.ligero_prover_binary_path.clone().unwrap().display()
        );
        tracing::info!("ligero argument: {}", ligero_argument_json);

        let ligero_prover_binary_path = self
            .ligero_prover_binary_path
            .clone()
            .unwrap()
            .canonicalize()
            .unwrap();

        tracing::info!(
            program = %ligero_program_path.display(),
            shader = %ligero_shader_path.display(),
            proof_dir = %unique_proof_dir.display(),
            packing,
            gpu_threads,
            "Starting Ligero prover process"
        );

        let mut child = Command::new(&ligero_prover_binary_path)
            .current_dir(&unique_proof_dir)
            .arg(&ligero_argument_json)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .inspect_err(|e| tracing::info!("failed to execute ligero prover: {:?}", e))
            .context("failed to execute ligero prover")?;

        let stdout = child
            .stdout
            .take()
            .context("failed to capture ligero stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture ligero stderr")?;

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        let stdout_buf_clone = stdout_buf.clone();
        let stdout_handle = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        tracing::info!(target = "ligero::stdout", "{}", line);
                        if let Ok(mut buf) = stdout_buf_clone.lock() {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target = "ligero::stdout", "Error reading stdout: {}", e);
                    }
                }
            }
        });

        let stderr_buf_clone = stderr_buf.clone();
        let stderr_handle = std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        tracing::warn!(target = "ligero::stderr", "{}", line);
                        if let Ok(mut buf) = stderr_buf_clone.lock() {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target = "ligero::stderr", "Error reading stderr: {}", e);
                    }
                }
            }
        });

        let status = child
            .wait()
            .inspect_err(|e| tracing::info!("ligero prover process wait failed: {:?}", e))
            .context("failed to wait for ligero prover process")?;

        // Ensure logging threads are drained
        let _ = stdout_handle.join();
        let _ = stderr_handle.join();

        let stdout_str = stdout_buf.lock().map(|s| s.clone()).unwrap_or_default();
        let stderr_str = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();

        tracing::info!(status = status.code(), "ligero prover execution finished");

        if !status.success() {
            anyhow::bail!(
                "ligero prover failed with status {:?}\nstdout: {}\nstderr: {}",
                status.code(),
                stdout_str,
                stderr_str
            );
        }

        if !stdout_str.contains("Final prove result:                  true") {
            tracing::info!("ligero prover did not produce a valid proof");
            anyhow::bail!(
                "ligero prover did not produce a valid proof\nstdout: {}\nstderr: {}",
                stdout_str,
                stderr_str
            );
        }

        tracing::info!("ligero prover generated successfully");

        // Read the proof from proof_data.gz (compressed - this goes into the transaction)
        let proof = std::fs::read(&proof_path).context("failed to read proof_data.gz")?;

        // Clean up the temporary directory after reading the proof
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
                Some(8000),
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
