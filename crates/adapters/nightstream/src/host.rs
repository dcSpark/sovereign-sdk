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

/// Default RAM size in bytes (64KB).
///
/// Sized to accommodate large serialized SpendPublic payloads echoed through
/// the placeholder circuit.
const DEFAULT_RAM_BYTES: usize = 0x10000;

/// Default chunk size (instructions per folding step).
///
/// The placeholder echo circuit executes ~10-50 instructions, so a small
/// chunk size keeps memory usage minimal.
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

    /// Write serialized bytes as input and set matching output claims.
    ///
    /// The placeholder echo circuit reads a length word followed by payload
    /// words from the input region and copies them verbatim to the output
    /// region starting at `OUTPUT_ADDR` (0x100).  This method:
    ///
    /// 1. Pads `data` to a 4-byte boundary.
    /// 2. Writes `len_words` (u32) as the first input word.
    /// 3. Writes each payload word as an input AND registers a matching
    ///    output claim so the prover/verifier enforce the echo.
    ///
    /// After proving, `extract_output_from_run` reconstructs the original
    /// bytes from the output claims.
    pub fn add_public_output_bytes(&mut self, data: &[u8]) {
        let mut padded = data.to_vec();
        while padded.len() % 4 != 0 {
            padded.push(0);
        }
        let len_words = (padded.len() / 4) as u32;

        // First input word: number of payload words the circuit should echo.
        self.add_u32_input(len_words);

        // Write each data word as input and set a matching output claim.
        let mut output_addr = 0x100u64; // OUTPUT_ADDR
        for chunk in padded.chunks_exact(4) {
            let word = u32::from_le_bytes(chunk.try_into().expect("chunk is 4 bytes"));
            self.add_u32_input(word);
            self.output_claims.push((output_addr, word as u64));
            output_addr += 4;
        }
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

            let output_value = self.extract_output_from_run(&run)?;

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

            let output_value = match self.try_simulate() {
                Ok(output) => output,
                Err(e) => {
                    tracing::warn!("Nightstream simulation failed: {}", e);
                    vec![]
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
    ///
    /// Reconstructs a byte buffer from all output claims, sorted by address.
    /// Each claim contributes one little-endian u32 word to the output.
    fn extract_output_from_run(
        &self,
        _run: &neo_fold::riscv_shard::Rv32B1Run,
    ) -> Result<Vec<u8>> {
        if self.output_claims.is_empty() {
            return Ok(vec![]);
        }
        // Sort claims by address to reconstruct bytes in order.
        let mut sorted: Vec<_> = self.output_claims.clone();
        sorted.sort_by_key(|&(addr, _)| addr);
        let mut bytes = Vec::with_capacity(sorted.len() * 4);
        for &(_, value) in &sorted {
            bytes.extend_from_slice(&(value as u32).to_le_bytes());
        }
        Ok(bytes)
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
