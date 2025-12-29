//! Host implementation for Ligero zkVM.
//!
//! Most of the implementation lives in `ligero-webgpu-runner` (Ligero-owned repo).
//! This adapter keeps a small wrapper so we can implement Sovereign traits without
//! running into Rust's orphan rules.

use anyhow::Result;
pub use ligero_webgpu_runner::{LigeroArg, LigeroConfig};
use ligero_webgpu_runner::sovereign_host::LigeroHostCore;
use serde::Serialize;
use sov_rollup_interface::zk::ZkvmHost;
use std::ops::{Deref, DerefMut};

use crate::{LigeroCodeCommitment, LigeroGuest, LigeroProofPackage};

/// Host for Ligero zkVM (Sovereign adapter wrapper).
#[derive(Clone, Debug)]
pub struct LigeroHost(LigeroHostCore);

impl Deref for LigeroHost {
    type Target = LigeroHostCore;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LigeroHost {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl LigeroHost {
    /// Create a new LigeroHost with the given WASM program path.
    pub fn new(program_path: &str) -> Self {
        Self(LigeroHostCore::new(program_path))
    }

    /// Set the packing size.
    pub fn with_packing(mut self, packing: u32) -> Self {
        self.0 = self.0.with_packing(packing);
        self
    }

    /// Set private argument indices (1-based).
    pub fn with_private_indices(mut self, indices: Vec<usize>) -> Self {
        self.0 = self.0.with_private_indices(indices);
        self
    }

    /// Set a custom identifier for the proof directory (for deterministic paths).
    pub fn with_proof_dir_id(mut self, id: String) -> Self {
        self.0 = self.0.with_proof_dir_id(id);
        self
    }

    /// Generate a proof and also return the prover's stdout for debugging.
    ///
    /// Returns `(serialized_proof_package, prover_stdout)`.
    pub fn run_with_logging(&mut self) -> Result<(Vec<u8>, String)> {
        let public_output = self.0.require_public_output()?;
        let (proof, stdout) = self.0.run_prover_with_output()?;

        let package = LigeroProofPackage {
            proof,
            public_output,
            args_json: serde_json::to_vec(&self.0.runner().config().args)?,
            private_indices: self.0.runner().config().private_indices.clone(),
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
    }

    fn code_commitment(
        &self,
    ) -> <<Self::Guest as sov_rollup_interface::zk::ZkvmGuest>::Verifier as sov_rollup_interface::zk::ZkVerifier>::CodeCommitment{
        LigeroCodeCommitment(self.0.code_commitment_raw())
    }

    fn run(&mut self, with_proof: bool) -> Result<Vec<u8>> {
        if with_proof {
            tracing::info!("Ligero: Generating proof with webgpu_prover");
            let public_output = self.0.require_public_output()?;
            let proof = self.0.run_prover()?;

            let package = LigeroProofPackage {
                proof,
                public_output,
                args_json: serde_json::to_vec(&self.0.runner().config().args)?,
                private_indices: self.0.runner().config().private_indices.clone(),
            };

            Ok(bincode::serialize(&package)?)
        } else {
            tracing::info!("Ligero: Executing without proof generation (simulation mode)");

            if let Err(e) = self.0.verify_proof_smoke() {
                tracing::warn!("Ligero execution check failed: {}", e);
            }

            let public_output = self
                .0
                .public_output_bytes()
                .map(|b| b.to_vec())
                .unwrap_or_default();

            let package = LigeroProofPackage {
                proof: vec![],
                public_output,
                args_json: serde_json::to_vec(&self.0.runner().config().args)?,
                private_indices: self.0.runner().config().private_indices.clone(),
            };

            Ok(bincode::serialize(&package)?)
        }
    }
}
