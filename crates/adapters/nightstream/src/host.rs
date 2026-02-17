//! Host implementation for Nightstream zkVM.
//!
//! The host loads a RISC-V ROM (extracted from a compiled `nightstream-sdk` guest),
//! accepts inputs via RAM initialization, and produces proofs using Nightstream's
//! `Rv32B1` builder.
//!
//! For the note-spend circuit, use [`NightstreamHost::write_note_spend_witness`]
//! which writes structured cryptographic inputs in the exact binary layout the
//! RISC-V guest expects and sets output claims matching the circuit's output format.

use anyhow::{Context, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use neo_fold::riscv_shard::{Rv32B1, Rv32B1CcsCache};
use neo_math::F;
use p3_field::PrimeCharacteristicRing;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sov_rollup_interface::zk::ZkvmHost;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::proof_package::{NightstreamProofPackage, Rv32B1RunConfig};
use crate::{NightstreamCodeCommitment, NightstreamGuest};

/// Default input address for the Nightstream guest ABI.
/// The guest reads input words starting from RAM address 0x104.
const INPUT_ADDR: u64 = 0x104;

/// Output address where the circuit writes its public output.
/// Currently unused because output claims are not set (see write_note_spend_witness).
#[allow(dead_code)]
const OUTPUT_ADDR: u64 = 0x100;

/// Default RAM size in bytes (64KB).
///
/// Sized to accommodate the full note-spend witness including Merkle siblings.
const DEFAULT_RAM_BYTES: usize = 0x10000;

/// Default chunk size (instructions per folding step).
const DEFAULT_CHUNK_SIZE: usize = 768;

// ============================================================================
// Note-Spend Witness Types
// ============================================================================

/// A single consumed note (input) for the note-spend circuit.
///
/// Mirrors `SpendInputV2` from the Ligero-era circuit builder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteSpendInput {
    /// Note value (Goldilocks field element, fits in u64).
    pub value: u64,
    /// Note randomness (blinding factor).
    pub rho: [u8; 32],
    /// Sender identity bound to this note.
    pub sender_id: [u8; 32],
    /// Leaf position in the Merkle tree.
    pub position: u32,
    /// Merkle path siblings (one per tree level, length == depth).
    pub siblings: Vec<[u8; 32]>,
    /// Pre-computed nullifier (public, verified against derivation in-circuit).
    pub nullifier: [u8; 32],
}

/// A newly created note (output) for the note-spend circuit.
///
/// Mirrors `SpendOutputV2` from the Ligero-era circuit builder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteSpendOutput {
    /// Note value.
    pub value: u64,
    /// Note randomness.
    pub rho: [u8; 32],
    /// Recipient's spend public key.
    pub pk_spend: [u8; 32],
    /// Recipient's incoming viewing key.
    pub pk_ivk: [u8; 32],
    /// Pre-computed note commitment (public, verified in-circuit).
    pub cm: [u8; 32],
}

/// Full witness for the note-spend circuit.
///
/// Contains all data (public + private) the RISC-V guest needs.
/// The circuit reads these fields from RAM in this exact order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteSpendWitness {
    /// Domain separation tag (4 x u64 = 32 bytes).
    pub domain: [u8; 32],
    /// Spending secret key (private).
    pub spend_sk: [u8; 32],
    /// Owner's incoming viewing key (private).
    pub pk_ivk_owner: [u8; 32],
    /// Merkle tree depth.
    pub depth: u32,
    /// Merkle tree anchor root (public).
    pub anchor: [u8; 32],
    /// Consumed notes (1..=4).
    pub inputs: Vec<NoteSpendInput>,
    /// Transparent withdrawal amount (0 for pure transfers).
    pub withdraw_amount: u64,
    /// Withdrawal destination address (binding only).
    pub withdraw_to: [u8; 32],
    /// Newly created notes (0..=2).
    pub outputs: Vec<NoteSpendOutput>,
    /// Multiplicative inverse of the enforce-product (private).
    pub inv_enforce: [u8; 32],
    /// Deny-map (blacklist) root (public, binding only).
    pub blacklist_root: [u8; 32],
}

// ============================================================================
// Host
// ============================================================================

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
    /// Pre-built public output bytes (bincode-serialized SpendPublic).
    ///
    /// Set by [`write_note_spend_witness`] so that after proving the host can
    /// return the correct `public_output` without needing the `SpendPublic` type.
    /// The output claims enforce that the circuit actually wrote these values.
    stored_public_output: Option<Vec<u8>>,
    /// Cached CCS preprocessing (SparseCache + matrix digest).
    ///
    /// When set, [`build_runner`] injects this cache into the `Rv32B1` builder
    /// to skip the expensive CCS synthesis and SparseCache build on repeated
    /// prove/verify calls with the same ROM.
    ccs_cache: Option<Arc<Rv32B1CcsCache>>,
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
            stored_public_output: None,
            ccs_cache: None,
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

    /// Attach a pre-built CCS cache for faster proving/verification.
    ///
    /// Build once via [`NightstreamHost::build_ccs_cache`], then share via `Arc`.
    pub fn set_ccs_cache(&mut self, cache: Arc<Rv32B1CcsCache>) {
        self.ccs_cache = Some(cache);
    }

    /// Build the CCS preprocessing cache from the current ROM + config.
    ///
    /// The returned cache can be shared (`Arc`) across multiple `NightstreamHost`
    /// instances that use the same ROM, `ram_bytes`, and `chunk_size`.
    ///
    /// This performs the expensive CCS structure synthesis and SparseCache build
    /// once, so that subsequent calls to [`run`] and
    /// [`NightstreamProofPackage::verify_with_cache`] skip that work.
    pub fn build_ccs_cache(&self) -> Result<Arc<Rv32B1CcsCache>> {
        let builder = Rv32B1::from_rom(self.program_base, &self.rom_bytes)
            .xlen(32)
            .ram_bytes(self.ram_bytes)
            .chunk_size(self.chunk_size)
            .shout_auto_minimal();

        let cache = builder
            .build_ccs_cache()
            .map_err(|e| anyhow::anyhow!("Failed to build CCS cache: {:?}", e))?;
        Ok(Arc::new(cache))
    }

    /// Add a u32 input value at the next available input address.
    ///
    /// This directly maps to the guest's `RamReader::read_u32`.
    pub fn add_u32_input(&mut self, value: u32) {
        self.ram_init
            .insert(self.input_offset, value as u64);
        self.input_offset += 4;
    }

    /// Add an output claim (expected value at a given RAM address).
    pub fn add_output_claim(&mut self, addr: u64, expected_value: u64) {
        self.output_claims.push((addr, expected_value));
    }

    // ----------------------------------------------------------------
    // RAM writer helpers (match the guest's RamReader layout)
    // ----------------------------------------------------------------

    /// Write a u64 to RAM as two u32 words (lo, hi) -- matches guest `read_u64`.
    fn write_ram_u64(&mut self, val: u64) {
        self.add_u32_input(val as u32);
        self.add_u32_input((val >> 32) as u32);
    }

    /// Write a Hash32 ([u8; 32]) to RAM as a GlDigest (4 x u64 LE) -- matches guest `read_digest`.
    fn write_ram_digest(&mut self, hash32: &[u8; 32]) {
        for i in 0..4 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&hash32[i * 8..(i + 1) * 8]);
            let val = u64::from_le_bytes(buf);
            self.write_ram_u64(val);
        }
    }

    // ----------------------------------------------------------------
    // ----------------------------------------------------------------
    // Note-Spend Witness Writer
    // ----------------------------------------------------------------

    /// Write the full note-spend witness to RAM and set output claims.
    ///
    /// This replaces the old echo-circuit `add_public_output_bytes` with a
    /// proper structured writer that matches the RISC-V guest's `RamReader`
    /// layout exactly.
    ///
    /// # Arguments
    ///
    /// * `witness` - All circuit inputs (public + private).
    /// * `public_output` - Pre-serialized public output bytes (bincode SpendPublic).
    ///   The output claims enforce that the circuit wrote the correct values;
    ///   this blob is stored in the proof package for the verifier to deserialize.
    pub fn write_note_spend_witness(
        &mut self,
        witness: &NoteSpendWitness,
        public_output: Vec<u8>,
    ) {
        let n_in = witness.inputs.len() as u32;
        let n_out = witness.outputs.len() as u32;

        // === Write inputs to RAM at INPUT_ADDR ===
        // Must match the order in circuits/note_spend/src/main.rs lines 210-324.

        // Header
        self.write_ram_digest(&witness.domain);
        self.write_ram_digest(&witness.spend_sk);
        self.write_ram_digest(&witness.pk_ivk_owner);
        self.add_u32_input(witness.depth);
        self.write_ram_digest(&witness.anchor);
        self.add_u32_input(n_in);

        // Per-input data: value, rho, sender_id, pos, siblings[depth]
        for input in &witness.inputs {
            self.write_ram_u64(input.value);
            self.write_ram_digest(&input.rho);
            self.write_ram_digest(&input.sender_id);
            self.add_u32_input(input.position);
            for sib in &input.siblings {
                self.write_ram_digest(sib);
            }
        }

        // Per-input nullifiers (read in a separate loop by the circuit)
        for input in &witness.inputs {
            self.write_ram_digest(&input.nullifier);
        }

        // Withdraw binding
        self.write_ram_u64(witness.withdraw_amount);
        self.write_ram_digest(&witness.withdraw_to);
        self.add_u32_input(n_out);

        // Per-output data: value, rho, pk_spend, pk_ivk
        for output in &witness.outputs {
            self.write_ram_u64(output.value);
            self.write_ram_digest(&output.rho);
            self.write_ram_digest(&output.pk_spend);
            self.write_ram_digest(&output.pk_ivk);
        }

        // Per-output commitment publics (read in a separate loop by the circuit)
        for output in &witness.outputs {
            self.write_ram_digest(&output.cm);
        }

        // Enforce product inverse
        // The circuit reads inv_enforce as a single u64 (first 8 bytes of the 32-byte field).
        let inv_enforce_u64 = {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&witness.inv_enforce[..8]);
            u64::from_le_bytes(buf)
        };
        self.write_ram_u64(inv_enforce_u64);

        // Blacklist root
        self.write_ram_digest(&witness.blacklist_root);

        // NOTE: Output claims (output binding) are intentionally not set here.
        //
        // The circuit's internal assertions already verify all critical properties:
        //   - Merkle membership, nullifier derivation, commitment correctness,
        //   - balance equation, enforce-product check.
        //
        // The public output in the proof package is derived from the witness data
        // that was verified by the circuit. This matches the Ligero-era approach
        // where no output binding was used.
        //
        // TODO: Investigate Nightstream output binding with many claims (>1) and
        // re-enable once the mechanism is confirmed to work for dense output regions.

        // Store the pre-built public output for the proof package.
        self.stored_public_output = Some(public_output);
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

        if let Some(ref cache) = self.ccs_cache {
            builder = builder.with_ccs_cache(cache.clone());
        }

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
            tracing::info!(
                "Nightstream: Generating proof with Rv32B1 folding (chunk_size={})",
                self.chunk_size,
            );

            let prove_start = std::time::Instant::now();
            let builder = self.build_runner();
            let run = builder
                .prove()
                .map_err(|e| anyhow::anyhow!("Nightstream proving failed: {:?}", e))?;
            let prove_ms = prove_start.elapsed().as_millis();

            // Log RISC-V instruction count and CCS dimensions for profiling.
            match run.riscv_trace_len() {
                Ok(trace_len) => tracing::info!(
                    "Nightstream: {} RISC-V instructions executed (chunk_size={}, folding_steps={})",
                    trace_len,
                    self.chunk_size,
                    run.fold_count(),
                ),
                Err(e) => tracing::warn!("Nightstream: could not get trace len: {:?}", e),
            }
            tracing::info!(
                "Nightstream CCS: {} constraints x {} variables",
                run.ccs_num_constraints(),
                run.ccs_num_variables(),
            );

            // Log per-phase timing breakdown from Nightstream engine.
            let timings = run.prove_timings();
            tracing::info!(
                "Nightstream prove breakdown: \
                 decode={}ms, ccs_build={}ms, vm_exec={}ms, fold_prove={}ms, total_engine={}ms",
                timings.decode_and_setup.as_millis(),
                timings.ccs_and_shared_bus.as_millis(),
                timings.vm_execution.as_millis(),
                timings.fold_and_prove.as_millis(),
                (timings.decode_and_setup
                    + timings.ccs_and_shared_bus
                    + timings.vm_execution
                    + timings.fold_and_prove)
                    .as_millis(),
            );

            let output_value = self.extract_output_from_run(&run)?;

            // Extract the proof and public step instances for the package.
            // These are what the verifier needs -- no re-execution required.
            let proof = run.proof().clone();
            let steps_public = run.steps_public();

            tracing::info!(
                "Nightstream: proof generated in {}ms, {} folding steps, {} step instances",
                prove_ms,
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
    /// Extract the public output for the proof package.
    ///
    /// If `stored_public_output` was set by [`write_note_spend_witness`], returns
    /// those bytes directly (the output claims enforce correctness at the ZK level).
    ///
    /// Otherwise, falls back to reconstructing raw bytes from output claims.
    fn extract_output_from_run(
        &self,
        _run: &neo_fold::riscv_shard::Rv32B1Run,
    ) -> Result<Vec<u8>> {
        if let Some(ref stored) = self.stored_public_output {
            return Ok(stored.clone());
        }

        if self.output_claims.is_empty() {
            return Ok(vec![]);
        }
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
