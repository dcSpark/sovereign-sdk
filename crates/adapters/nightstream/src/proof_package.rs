//! Proof package for Nightstream proofs.
//!
//! Defines `NightstreamProofPackage` and `Rv32B1RunConfig` locally so the
//! sovereign-ligero adapter is self-contained -- no `sovereign_bridge` module
//! in the Nightstream crate is needed.

use neo_ajtai::Commitment as Cmt;
use neo_fold::riscv_shard::{Rv32B1, Rv32B1CcsCache};
use neo_fold::shard::ShardProof;
use neo_fold::PiCcsError;
use neo_math::{F, K};
use neo_memory::witness::StepInstanceBundle;
use p3_field::PrimeCharacteristicRing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Configuration needed to reconstruct verification context from ROM bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rv32B1RunConfig {
    /// Program base address (must be 0 for current Nightstream).
    pub program_base: u64,
    /// Word size (must be 32).
    pub xlen: usize,
    /// RAM size in bytes.
    pub ram_bytes: usize,
    /// Instructions per folding chunk.
    pub chunk_size: usize,
    /// Initial RAM values: address -> value.
    pub ram_init: HashMap<u64, u64>,
    /// Output claims: (address, expected_value_as_u64).
    pub output_claims: Vec<(u64, u64)>,
}

/// A self-contained proof package for Nightstream RV32 proofs.
///
/// Contains everything needed for an external verifier to check the proof
/// without access to the original proving session.
///
/// The package includes:
/// - **`proof`**: the folding proof (`ShardProof`) produced by the prover.
/// - **`steps_public`**: the public step instance bundles from the proving session.
///   These carry the per-step MCS commitments, public inputs, and memory/lookup
///   instances that the verifier checks the proof against.
/// - **`rom_bytes`** + **`config`**: enough to reconstruct the CCS structure
///   (circuit) on the verifier side.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NightstreamProofPackage {
    /// The folding proof (serialized via serde).
    pub proof: ShardProof,

    /// Public step instance bundles produced during proving.
    ///
    /// These carry per-step MCS commitments + public inputs, plus memory and
    /// lookup instances.  They are produced by `Rv32B1Run::steps_public()`
    /// after a successful `prove()`.
    pub steps_public: Vec<StepInstanceBundle<Cmt, F, K>>,

    /// Public output bytes (bincode-serialized application output).
    pub public_output: Vec<u8>,

    /// ROM bytes (the `.neo_start` section of the guest ELF).
    /// Needed to reconstruct the CCS structure for verification.
    pub rom_bytes: Vec<u8>,

    /// Run configuration needed to reconstruct verification context.
    pub config: Rv32B1RunConfig,
}

impl NightstreamProofPackage {
    /// Serialize this package to bytes (using bincode).
    pub fn to_bytes(&self) -> Result<Vec<u8>, PiCcsError> {
        bincode::serialize(self)
            .map_err(|e| PiCcsError::InvalidInput(format!("proof package serialization failed: {e}")))
    }

    /// Deserialize a package from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PiCcsError> {
        bincode::deserialize(bytes)
            .map_err(|e| PiCcsError::InvalidInput(format!("proof package deserialization failed: {e}")))
    }

    /// Verify this proof package (verify-only, no re-execution).
    ///
    /// Reconstructs the RV32 B1 verification context (CCS + session) from the
    /// ROM and config, then verifies the embedded `ShardProof` against the
    /// provided `steps_public` instances.
    ///
    /// **No RISC-V execution or re-proving is performed.**  The cost is circuit
    /// synthesis (~ms) plus the IOP verification check.
    pub fn verify(&self) -> Result<bool, PiCcsError> {
        self.verify_inner(None)
    }

    /// Verify using a pre-built CCS cache (skips expensive CCS synthesis).
    ///
    /// The cache must have been built from a builder with the same ROM,
    /// `ram_bytes`, and `chunk_size` as this package's config.
    ///
    /// Build one via [`crate::NightstreamHost::build_ccs_cache`], then share
    /// across all verification calls.
    pub fn verify_with_cache(&self, cache: &Arc<Rv32B1CcsCache>) -> Result<bool, PiCcsError> {
        self.verify_inner(Some(cache))
    }

    fn verify_inner(&self, cache: Option<&Arc<Rv32B1CcsCache>>) -> Result<bool, PiCcsError> {
        let mut builder = Rv32B1::from_rom(self.config.program_base, &self.rom_bytes)
            .xlen(self.config.xlen)
            .ram_bytes(self.config.ram_bytes)
            .chunk_size(self.config.chunk_size)
            .shout_auto_minimal();

        if let Some(c) = cache {
            builder = builder.with_ccs_cache((*c).clone());
        }

        for (&addr, &value) in &self.config.ram_init {
            builder = builder.ram_init_u32(addr, value as u32);
        }

        for &(addr, value) in &self.config.output_claims {
            builder = builder.output_claim(addr, F::from_u64(value));
        }

        // Build verify-only context (CCS + session, no execution).
        let mut verifier = builder.build_verifier()?;

        // Preload the verifier SparseCache AFTER build so the pointer-keyed
        // cache uses the final CCS address (inside the Rv32B1Verifier struct).
        if let Some(c) = cache {
            if let Some(digest) = &c.ccs_mat_digest {
                verifier.preload_sparse_cache_with_digest(c.sparse.clone(), digest.clone())?;
            } else {
                verifier.preload_sparse_cache(c.sparse.clone())?;
            }
        }

        // Verify the ShardProof against the prover-supplied step instances.
        verifier.verify(&self.proof, &self.steps_public)
    }
}
