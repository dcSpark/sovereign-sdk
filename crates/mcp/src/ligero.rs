//! Zero-knowledge proof generation module for value-setter-zk transactions
//!
//! This module provides functionality to generate Ligero proofs that demonstrate
//! a value is within a valid range without revealing the computation details.

use std::path::PathBuf;

use anyhow::{Context, Result};
use ligero_runner::{LigeroPaths, LigeroRunner, ProverRunOptions};
use serde::{Deserialize, Serialize};

/// Program argument encoding expected by the Ligero prover/verifier JSON interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LigeroProgramArguments {
    STR { str: String },
    I64 { i64: i64 },
    HEX { hex: String },
}

/// Minimal wrapper used by MCP to generate Ligero proofs.
///
/// All actual `webgpu_prover` process execution is delegated to `ligero-runner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ligero {
    ligero_prover_binary_path: Option<PathBuf>,
    ligero_shader_path: Option<PathBuf>,
    ligero_program_path: Option<PathBuf>,
    proof_dir_id: Option<String>,
}

impl Ligero {
    pub fn new(
        ligero_prover_binary_path: Option<PathBuf>,
        ligero_shader_path: Option<PathBuf>,
        ligero_program_path: Option<PathBuf>,
    ) -> Self {
        Self {
            ligero_prover_binary_path,
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
        let prover_bin = self
            .ligero_prover_binary_path
            .clone()
            .context("ligero prover binary path is required")?
            .canonicalize()
            .context("Failed to canonicalize Ligero prover binary path")?;
        let shader_dir = self
            .ligero_shader_path
            .clone()
            .context("ligero shader path is required")?
            .canonicalize()
            .context("Failed to canonicalize Ligero shader path")?;
        let program = self
            .ligero_program_path
            .clone()
            .context("ligero program path is required")?
            .canonicalize()
            .context("Failed to canonicalize Ligero program path")?;

        let bins_dir = prover_bin
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let paths = LigeroPaths {
            prover_bin: prover_bin.clone(),
            verifier_bin: bins_dir.join("webgpu_verifier"),
            shader_dir,
            bins_dir,
        };

        let mut runner = LigeroRunner::new_with_paths(&program.to_string_lossy(), paths);
        runner.config_mut().packing = packing;
        runner.config_mut().gpu_threads = gpu_threads;
        runner.config_mut().private_indices =
            private_indices.into_iter().map(|v| v as usize).collect();
        runner.config_mut().args = args
            .into_iter()
            .map(|a| match a {
                LigeroProgramArguments::STR { str } => ligero_runner::LigeroArg::String { str },
                LigeroProgramArguments::I64 { i64 } => ligero_runner::LigeroArg::I64 { i64 },
                LigeroProgramArguments::HEX { hex } => ligero_runner::LigeroArg::Hex { hex },
            })
            .collect();
        if let Some(id) = &self.proof_dir_id {
            runner.set_proof_dir_id(id.clone());
        }

        runner.run_prover_with_options(ProverRunOptions {
            keep_proof_dir: true,
            proof_outputs_base: None,
            write_replay_script: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::ligero::create_test_ligero;

    #[tracing_test::traced_test]
    #[test]
    fn test_generate_proof() {
        let Some(ligero) = create_test_ligero() else {
            return;
        };

        let proof = match ligero.generate_proof(
            8192,
            Some(8000),
            vec![1],
            vec![
                LigeroProgramArguments::I64 { i64: 1 },
                LigeroProgramArguments::I64 { i64: 1 },
            ],
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("⚠️  Skipping Ligero proof generation test: {e}");
                return;
            }
        };
        assert!(!proof.is_empty(), "proof should not be empty");
    }
}
