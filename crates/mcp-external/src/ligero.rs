//! Zero-knowledge proof generation module for value-setter-zk transactions
//!
//! This module provides functionality to generate Ligero proofs that demonstrate
//! a value is within a valid range without revealing the computation details.

use std::path::PathBuf;

use anyhow::{Context, Result};
use ligero_runner::{LigeroPaths, LigeroRunner, ProverRunOptions};
use serde::{Deserialize, Serialize};

/// Program argument encoding expected by the Ligero prover/verifier JSON interface.
pub use ligero_runner::LigeroArg as LigeroProgramArguments;

/// Minimal wrapper used by MCP to generate Ligero proofs.
///
/// All actual `webgpu_prover` process execution is delegated to `ligero-runner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ligero {
    ligero_prover_binary_path: Option<PathBuf>,
    ligero_shader_path: Option<PathBuf>,
    /// Program specifier: circuit name (preferred) or a full `.wasm` path.
    ligero_program: Option<String>,
    proof_dir_id: Option<String>,
}

impl Ligero {
    pub fn new(
        ligero_prover_binary_path: Option<PathBuf>,
        ligero_shader_path: Option<PathBuf>,
        ligero_program: Option<String>,
    ) -> Self {
        Self {
            ligero_prover_binary_path,
            ligero_shader_path,
            ligero_program,
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
            .map(|s| LigeroProgramArguments::String { str: s })
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
        let program = self
            .ligero_program
            .clone()
            .or_else(|| std::env::var("LIGERO_PROGRAM_PATH").ok())
            .context(
                "ligero program is required (config.ligero_program_path or LIGERO_PROGRAM_PATH)",
            )?;

        let mut runner = if self.ligero_prover_binary_path.is_some()
            || self.ligero_shader_path.is_some()
        {
            // Explicit overrides (backwards compatible with existing MCP config).
            let prover_bin = self
                .ligero_prover_binary_path
                .clone()
                .or_else(|| {
                    std::env::var("LIGERO_PROVER_BIN")
                        .ok()
                        .or_else(|| std::env::var("LIGERO_PROVER_BINARY_PATH").ok())
                        .map(PathBuf::from)
                })
                .context("ligero prover binary path is required (config.ligero_prover_binary_path or LIGERO_PROVER_BIN/LIGERO_PROVER_BINARY_PATH)")?
                .canonicalize()
                .context("Failed to canonicalize Ligero prover binary path")?;

            let shader_dir = self
                .ligero_shader_path
                .clone()
                .or_else(|| std::env::var("LIGERO_SHADER_PATH").ok().map(PathBuf::from))
                .context("ligero shader path is required (config.ligero_shader_path or LIGERO_SHADER_PATH)")?
                .canonicalize()
                .context("Failed to canonicalize Ligero shader path")?;

            let bins_dir = prover_bin
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));

            let verifier_bin = std::env::var("LIGERO_VERIFIER_BIN")
                .ok()
                .map(PathBuf::from)
                .unwrap_or_else(|| bins_dir.join("webgpu_verifier"));

            let paths = LigeroPaths {
                prover_bin: prover_bin.clone(),
                verifier_bin,
                shader_dir,
                bins_dir,
            };

            LigeroRunner::new_with_paths(&program, paths)
        } else {
            // Prefer runner auto-discovery (uses env overrides + git checkout discovery).
            LigeroRunner::new(&program)
        };
        runner.config_mut().packing = packing;
        // Default to raw proofs (no gzip) to avoid compression overhead during proving.
        runner.config_mut().gzip_proof = false;
        runner.config_mut().gpu_threads = gpu_threads;
        runner.config_mut().private_indices =
            private_indices.into_iter().map(|v| v as usize).collect();
        runner.config_mut().args = args;
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
