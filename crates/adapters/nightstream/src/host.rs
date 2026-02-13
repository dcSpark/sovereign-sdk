//! Host implementation for Nightstream zkVM.
//!
//! The host loads a RISC-V ROM (extracted from a compiled `nightstream-sdk` guest),
//! accepts inputs via RAM initialization, and produces proofs using Nightstream's
//! `Rv32B1` builder.

use anyhow::{Context, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use neo_fold::riscv_shard::Rv32B1;
use neo_math::F;
use p3_field::PrimeCharacteristicRing;
use serde::Serialize;
use sha2::Digest;
use sov_rollup_interface::zk::ZkvmHost;
use std::collections::HashMap;
use std::io::Write;

use crate::proof_package::{NightstreamProofPackage, Rv32B1RunConfig};
use crate::{NightstreamCodeCommitment, NightstreamGuest};

/// Default input address for the Nightstream guest ABI.
/// The guest reads input words starting from RAM address 0x104.
const INPUT_ADDR: u64 = 0x104;

/// Default RAM size in bytes.
const DEFAULT_RAM_BYTES: usize = 0x800;

/// Default chunk size (instructions per folding step).
const DEFAULT_CHUNK_SIZE: usize = 16;

/// Host for Nightstream zkVM (Sovereign adapter).
///
/// Wraps the Nightstream `Rv32B1` builder, translating between the Sovereign SDK
/// `ZkvmHost` interface and Nightstream's RISC-V proving pipeline.
#[derive(Clone, Debug)]
pub struct NightstreamHost {
    /// ROM bytes from the `.neo_start` ELF section.
    rom_bytes: Vec<u8>,
    /// Program base address (from .neo_start section header).
    program_base: u64,
    /// RAM initialization values: address -> value (u32).
    ram_init: HashMap<u64, u64>,
    /// Current write offset for `add_hint` (tracks next free RAM address for inputs).
    input_offset: u64,
    /// RAM size in bytes.
    ram_bytes: usize,
    /// Chunk size for folding.
    chunk_size: usize,
    /// Output claims: (address, expected_value).
    output_claims: Vec<(u64, u64)>,
    /// Optional caller-provided public output bytes.
    ///
    /// When set, `run()` stores these bytes as `public_output` in the proof package
    /// instead of extracting the output from the VM's output claims. This is used
    /// for pass-through circuits where the host pre-computes the public output
    /// (e.g. `SpendPublic` for the placeholder note_spend circuit).
    custom_public_output: Option<Vec<u8>>,
}

impl NightstreamHost {
    /// Create a new NightstreamHost from ROM bytes and program base address.
    pub fn new(rom_bytes: &[u8], program_base: u64) -> Self {
        Self {
            rom_bytes: rom_bytes.to_vec(),
            program_base,
            ram_init: HashMap::new(),
            input_offset: INPUT_ADDR,
            ram_bytes: DEFAULT_RAM_BYTES,
            chunk_size: DEFAULT_CHUNK_SIZE,
            output_claims: Vec::new(),
            custom_public_output: None,
        }
    }

    /// Set the RAM size in bytes.
    pub fn with_ram_bytes(mut self, ram_bytes: usize) -> Self {
        self.ram_bytes = ram_bytes;
        self
    }

    /// Set the chunk size (instructions per folding step).
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Add a u32 input value at the next available input address.
    ///
    /// This directly maps to `NeoAbi::write_to_words` on the guest side.
    pub fn add_u32_input(&mut self, value: u32) {
        self.ram_init
            .insert(self.input_offset, value as u64);
        self.input_offset += 4;
    }

    /// Add an output claim (expected value at a given RAM address).
    pub fn add_output_claim(&mut self, addr: u64, expected_value: u64) {
        self.output_claims.push((addr, expected_value));
    }

    /// Set a custom public output to embed in the proof package.
    ///
    /// When set, `run()` uses these bytes as `public_output` in the
    /// [`NightstreamProofPackage`] instead of extracting the output from the
    /// VM's output claims. This is useful for pass-through circuits where the
    /// host pre-computes the public output (e.g. a bincode-serialized
    /// `SpendPublic` for the placeholder note_spend circuit).
    pub fn set_custom_public_output(&mut self, output: Vec<u8>) {
        self.custom_public_output = Some(output);
    }

    /// Compute the SHA-256 code commitment of the ROM bytes.
    pub fn compute_commitment(&self) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        hasher.update(&self.rom_bytes);
        hasher.finalize().into()
    }

    /// Build the `Rv32B1` runner from the current configuration.
    fn build_runner(&self) -> Rv32B1 {
        let mut builder = Rv32B1::from_rom(self.program_base, &self.rom_bytes)
            .xlen(32)
            .ram_bytes(self.ram_bytes)
            .chunk_size(self.chunk_size)
            .shout_auto_minimal();

        for (&addr, &value) in &self.ram_init {
            builder = builder.ram_init_u32(addr, value as u32);
        }

        for &(addr, value) in &self.output_claims {
            builder = builder.output_claim(addr, F::from_u64(value));
        }

        builder
    }

    /// Build the run configuration for proof packaging.
    fn build_config(&self) -> Rv32B1RunConfig {
        Rv32B1RunConfig {
            program_base: self.program_base,
            xlen: 32,
            ram_bytes: self.ram_bytes,
            chunk_size: self.chunk_size,
            ram_init: self.ram_init.clone(),
            output_claims: self.output_claims.clone(),
        }
    }
}

/// Host arguments for Nightstream: ROM bytes + program base address.
#[derive(Clone, Debug, Default)]
pub struct NightstreamHostArgs {
    /// ROM bytes from the `.neo_start` ELF section.
    pub rom_bytes: Vec<u8>,
    /// Program base address (from .neo_start section header).
    pub program_base: u64,
}

impl NightstreamHostArgs {
    /// Create new host args from ROM bytes and base address.
    pub fn new(rom_bytes: Vec<u8>, program_base: u64) -> Self {
        Self {
            rom_bytes,
            program_base,
        }
    }
}

impl ZkvmHost for NightstreamHost {
    type Guest = NightstreamGuest;
    type HostArgs = NightstreamHostArgs;

    fn from_args(args: &Self::HostArgs) -> Self {
        Self::new(&args.rom_bytes, args.program_base)
    }

    fn add_hint<T: Serialize>(&mut self, item: T) {
        // Serialize the hint to bytes and pack as u32 words into RAM at the input address.
        let bytes = bincode::serialize(&item).expect("failed to serialize hint");

        // Pad to 4-byte boundary
        let mut padded = bytes;
        while padded.len() % 4 != 0 {
            padded.push(0);
        }

        for chunk in padded.chunks_exact(4) {
            let word = u32::from_le_bytes(chunk.try_into().expect("chunk is 4 bytes"));
            self.ram_init
                .insert(self.input_offset, word as u64);
            self.input_offset += 4;
        }
    }

    fn code_commitment(
        &self,
    ) -> <<Self::Guest as sov_rollup_interface::zk::ZkvmGuest>::Verifier as sov_rollup_interface::zk::ZkVerifier>::CodeCommitment
    {
        NightstreamCodeCommitment(self.compute_commitment())
    }

    fn run(&mut self, with_proof: bool) -> Result<Vec<u8>> {
        if with_proof {
            tracing::info!("Nightstream: Generating proof with Rv32B1 folding");

            let builder = self.build_runner();
            let run = builder
                .prove()
                .map_err(|e| anyhow::anyhow!("Nightstream proving failed: {:?}", e))?;

            // Use caller-provided public output if set, otherwise extract from run.
            let output_value = if let Some(ref custom) = self.custom_public_output {
                custom.clone()
            } else {
                self.extract_output_from_run(&run)?
            };

            // Extract the proof and public step instances for the package.
            // These are what the verifier needs -- no re-execution required.
            let proof = run.proof().clone();
            let steps_public = run.steps_public();

            tracing::info!(
                "Nightstream: proof generated, {} folding steps, {} step instances",
                proof.steps.len(),
                steps_public.len(),
            );

            let package = NightstreamProofPackage {
                proof,
                steps_public,
                public_output: output_value,
                rom_bytes: self.rom_bytes.clone(),
                config: self.build_config(),
            };

            // Log per-field size breakdown for diagnostics
            if let (Ok(sz_proof), Ok(sz_steps), Ok(sz_rom), Ok(sz_config), Ok(sz_output)) = (
                bincode::serialized_size(&package.proof),
                bincode::serialized_size(&package.steps_public),
                bincode::serialized_size(&package.rom_bytes),
                bincode::serialized_size(&package.config),
                bincode::serialized_size(&package.public_output),
            ) {
                let total = sz_proof + sz_steps + sz_rom + sz_config + sz_output;
                tracing::info!(
                    "Nightstream proof package size breakdown: \
                     proof={:.2}MB, steps_public={:.2}MB, rom={:.1}KB, \
                     config={:.1}KB, output={} bytes, total={:.2}MB",
                    sz_proof as f64 / 1_048_576.0,
                    sz_steps as f64 / 1_048_576.0,
                    sz_rom as f64 / 1_024.0,
                    sz_config as f64 / 1_024.0,
                    sz_output,
                    total as f64 / 1_048_576.0,
                );
            }

            let raw = bincode::serialize(&package)
                .context("Failed to serialize NightstreamProofPackage")?;
            let compressed = compress_proof_bytes(&raw)?;
            tracing::info!(
                "Nightstream proof compressed: {:.2}MB -> {:.2}MB ({:.0}% reduction)",
                raw.len() as f64 / 1_048_576.0,
                compressed.len() as f64 / 1_048_576.0,
                (1.0 - compressed.len() as f64 / raw.len() as f64) * 100.0,
            );
            Ok(compressed)
        } else {
            tracing::info!("Nightstream: Executing without proof generation (simulation mode)");

            // Use caller-provided public output if set, otherwise simulate to extract.
            let output_value = if let Some(ref custom) = self.custom_public_output {
                custom.clone()
            } else {
                match self.try_simulate() {
                    Ok(output) => output,
                    Err(e) => {
                        tracing::warn!("Nightstream simulation failed: {}", e);
                        vec![]
                    }
                }
            };

            // For simulation mode, run the prover to get a valid package structure.
            // In the future, this could use a lighter-weight execution path.
            let builder = self.build_runner();
            let run = builder
                .prove()
                .map_err(|e| anyhow::anyhow!("Nightstream simulation prove failed: {:?}", e))?;
            let proof = run.proof().clone();
            let steps_public = run.steps_public();

            let package = NightstreamProofPackage {
                proof,
                steps_public,
                public_output: output_value,
                rom_bytes: self.rom_bytes.clone(),
                config: self.build_config(),
            };

            let raw = bincode::serialize(&package)
                .context("Failed to serialize NightstreamProofPackage")?;
            compress_proof_bytes(&raw)
        }
    }
}

/// Compress proof bytes with DEFLATE for efficient transport.
fn compress_proof_bytes(raw: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(raw)
        .context("Failed to write proof bytes to deflate encoder")?;
    encoder
        .finish()
        .context("Failed to finish deflate compression")
}

impl NightstreamHost {
    /// Extract the public output from a completed Rv32B1 run.
    fn extract_output_from_run(
        &self,
        _run: &neo_fold::riscv_shard::Rv32B1Run,
    ) -> Result<Vec<u8>> {
        // The output is determined by what the guest wrote to RAM at OUTPUT_ADDR.
        // For the Sovereign SDK integration, we serialize the expected output from
        // the output claims that were configured.
        if let Some(&(_, value)) = self.output_claims.first() {
            bincode::serialize(&(value as u32))
                .context("Failed to serialize output value")
        } else {
            Ok(vec![])
        }
    }

    /// Try a simulation run (no proof, just execution) to extract output.
    fn try_simulate(&self) -> Result<Vec<u8>> {
        let builder = self.build_runner();
        let run = builder
            .prove()
            .map_err(|e| anyhow::anyhow!("Nightstream simulation failed: {:?}", e))?;

        self.extract_output_from_run(&run)
    }
}
