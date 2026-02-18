//! Proof package for Nightstream proofs.
//!
//! Defines `NightstreamProofPackage` and `Rv32TraceWiringRunConfig` locally so the
//! sovereign-ligero adapter is self-contained -- no `sovereign_bridge` module
//! in the Nightstream crate is needed.

use neo_ajtai::Commitment as Cmt;
use neo_fold::riscv_trace_shard::Rv32TraceWiring;
use neo_fold::shard::ShardProof;
use neo_fold::PiCcsError;
use neo_math::{F, K};
use neo_memory::witness::StepInstanceBundle;
use p3_field::PrimeCharacteristicRing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration needed to reconstruct verification context from ROM bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rv32TraceWiringRunConfig {
    /// Program base address (must be 0 for current Nightstream).
    pub program_base: u64,
    /// Word size (must be 32).
    pub xlen: usize,
    /// Rows per trace step (chunk rows) as used during proving.
    pub step_rows: usize,
    /// RAM address width (bits) as determined during proving.
    pub ram_d: usize,
    /// Width-lookup address bits (0 when absent).
    pub width_lookup_addr_d: usize,
    /// Initial RAM values: address -> value.
    pub ram_init: HashMap<u64, u64>,
    /// Initial register values: register index -> value.
    pub reg_init: HashMap<u64, u64>,
    /// Output claims: (address, expected_value_as_u64).
    pub output_claims: Vec<(u64, u64)>,
}

/// Pool-operator Ed25519 signature over a viewer FVK commitment.
///
/// When `POOL_FVK_PK` is configured, the proof verifier service requires this
/// to be present in the proof package so it can verify that the viewer
/// attestations use a pool-authorized FVK.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolViewerSig {
    /// The viewer FVK commitment that was signed (`H("FVK_COMMIT_V1" || fvk)`).
    pub fvk_commitment: [u8; 32],
    /// Ed25519 signature bytes (64 bytes) over `fvk_commitment` by the pool operator key.
    pub signature: Vec<u8>,
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
    /// lookup instances.  They are produced by `Rv32TraceWiringRun::steps_public()`
    /// after a successful `prove()`.
    pub steps_public: Vec<StepInstanceBundle<Cmt, F, K>>,

    /// Public output bytes (bincode-serialized application output).
    pub public_output: Vec<u8>,

    /// ROM bytes (the `.neo_start` section of the guest ELF).
    /// Needed to reconstruct the CCS structure for verification.
    pub rom_bytes: Vec<u8>,

    /// Run configuration needed to reconstruct verification context.
    pub config: Rv32TraceWiringRunConfig,

    /// Optional pool-operator signature over the viewer FVK commitment.
    ///
    /// When the pool operator requires Level-B viewer enforcement, this carries
    /// the Ed25519 signature that the proof verifier service checks before
    /// accepting the proof.
    #[serde(default)]
    pub pool_viewer_sig: Option<PoolViewerSig>,
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
    /// Reconstructs the RV32 trace-wiring verification context (CCS + session)
    /// from the ROM and config, then verifies the embedded `ShardProof` against
    /// the provided `steps_public` instances.
    ///
    /// **No RISC-V execution or re-proving is performed.**  The cost is circuit
    /// synthesis (~ms) plus the IOP verification check.
    pub fn verify(&self) -> Result<bool, PiCcsError> {
        let mut builder = Rv32TraceWiring::from_rom(self.config.program_base, &self.rom_bytes)
            .xlen(self.config.xlen)
            .shout_auto_minimal();

        for (&addr, &value) in &self.config.ram_init {
            builder = builder.ram_init_u32(addr, value as u32);
        }

        for (&reg, &value) in &self.config.reg_init {
            builder = builder.reg_init_u32(reg, value as u32);
        }

        for &(addr, value) in &self.config.output_claims {
            builder = builder.output_claim(addr, F::from_u64(value));
        }

        let verifier = builder.build_verifier(
            self.config.step_rows,
            self.config.ram_d,
            self.config.width_lookup_addr_d,
        )?;

        verifier.verify(&self.proof, &self.steps_public)
    }
}
