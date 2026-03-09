//! Host implementation for Nightstream zkVM.
//!
//! The host loads RV64IM guest bytes (currently the full ELF), accepts inputs via
//! RAM initialization, and produces proofs using Nightstream's
//! `Rv64TraceWiring` builder.
//!
//! For the note-spend circuit, use [`NightstreamHost::write_note_spend_witness`]
//! which writes structured cryptographic inputs in the exact binary layout the
//! RISC-V guest expects and sets output claims matching the circuit's output format.

use anyhow::{Context, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use neo_fold::rv64_trace_shard::{Rv64TraceWiring, Rv64TraceWiringRun};
use neo_math::F;
use p3_field::PrimeCharacteristicRing;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sov_rollup_interface::zk::ZkvmHost;
use std::collections::HashMap;
use std::io::Write;

use crate::circuit_output::{
    deposit_public_bytes_from_circuit_output, deposit_public_bytes_from_output_claims,
    output_claims_to_bytes, spend_public_bytes_from_circuit_output,
    spend_public_bytes_from_output_claims, CircuitOutput, CircuitViewAttestation,
    DepositCircuitOutput,
};
use crate::proof_package::{
    encode_shard_proof_bytes, NightstreamProofPackage, PublicOutputFormat, Rv64TraceWiringRunConfig,
};
use crate::{NightstreamCodeCommitment, NightstreamGuest};

/// Default input address for generic guest ABIs.
const DEFAULT_INPUT_ADDR: u64 = 0x108;

/// Note-circuit input base. Kept away from low-memory startup zeroing.
const NOTE_CIRCUIT_INPUT_ADDR: u64 = 0x4104;

/// Note-circuit output base.
const NOTE_CIRCUIT_OUTPUT_ADDR: u64 = 0x4100;

/// Default chunk rows (rows per trace-wiring folding step).
const DEFAULT_CHUNK_ROWS: usize = 1 << 16;

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

/// Blacklist non-membership proof for a single identity.
///
/// Contains the bucket entries, inverse witness, and Merkle siblings needed
/// by the circuit's `assert_not_blacklisted` function.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlacklistProof {
    /// The 12 entries in the deny-map bucket (BL_BUCKET_SIZE = 12).
    pub bucket_entries: [[u8; 32]; 12],
    /// Multiplicative inverse of product(id - entry) over all bucket entries.
    /// Stored as a 32-byte field; the circuit reads only the first 8 bytes as u64.
    pub bucket_inv: [u8; 32],
    /// Merkle path siblings from the bucket leaf to the blacklist root (BL_DEPTH = 16).
    pub siblings: Vec<[u8; 32]>,
}

/// Blacklist tree constants (must match the circuit).
const BL_DEPTH: u8 = 16;
const BL_BUCKET_SIZE: usize = 12;
const TAG_BL_BUCKET: u64 = 7;
const TAG_MT_NODE_BL: u64 = 1;
const TAG_ADDR: u64 = 5;

impl BlacklistProof {
    /// Build a non-membership proof from on-chain deny-map opening data.
    ///
    /// `id` is the 32-byte recipient identity (the deny-map key).
    /// `bucket_entries` and `siblings` come from `BlacklistOpeningResponse`.
    /// The inverse witness is computed from the identity and bucket entries.
    pub fn from_opening(
        id: &[u8; 32],
        bucket_entries: [[u8; 32]; BL_BUCKET_SIZE],
        siblings: Vec<[u8; 32]>,
    ) -> Self {
        let bucket_inv = compute_bucket_inv(id, &bucket_entries);
        Self {
            bucket_entries,
            bucket_inv,
            siblings,
        }
    }

    /// Build a non-membership proof for `id` against the default (all-empty) deny-map.
    ///
    /// Use this when the blacklist root is [`default_blacklist_root()`].
    pub fn default_for_identity(id: &[u8; 32]) -> Self {
        let bucket_entries = [[0u8; 32]; BL_BUCKET_SIZE];
        let bucket_inv = compute_bucket_inv(id, &bucket_entries);

        let default_nodes = compute_default_bl_nodes();
        let siblings: Vec<[u8; 32]> = default_nodes[..BL_DEPTH as usize]
            .iter()
            .map(gl_digest_to_hash32)
            .collect();

        Self {
            bucket_entries,
            bucket_inv,
            siblings,
        }
    }
}

/// Compute the bucket inverse witness: `1 / product(id - entry)` for all entries.
///
/// This matches the circuit's `enforce_prod_digest_diff` logic:
/// for each entry, multiply `prod *= (id[0]-e[0]) * (id[1]-e[1]) * (id[2]-e[2]) * (id[3]-e[3])`.
fn compute_bucket_inv(id: &[u8; 32], bucket_entries: &[[u8; 32]; BL_BUCKET_SIZE]) -> [u8; 32] {
    use p3_field::{Field, PrimeCharacteristicRing, PrimeField64};
    use p3_goldilocks::Goldilocks;

    let id_gl = hash32_to_gl(id);

    let mut prod = Goldilocks::ONE;
    for entry in bucket_entries {
        let e_gl = hash32_to_gl(entry);
        for i in 0..4 {
            prod *= id_gl[i] - e_gl[i];
        }
    }

    let inv = prod.inverse();
    let mut bucket_inv = [0u8; 32];
    bucket_inv[..8].copy_from_slice(&inv.as_canonical_u64().to_le_bytes());
    bucket_inv
}

/// Convert a Hash32 (32 bytes LE) to 4 Goldilocks field elements.
fn hash32_to_gl(h: &[u8; 32]) -> [p3_goldilocks::Goldilocks; 4] {
    use p3_field::PrimeCharacteristicRing;
    use p3_goldilocks::Goldilocks;

    let mut d = [Goldilocks::ZERO; 4];
    for i in 0..4 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&h[i * 8..(i + 1) * 8]);
        d[i] = Goldilocks::from_u64(u64::from_le_bytes(buf));
    }
    d
}

/// Compute the default (all-allowed) deny-map Merkle root.
///
/// This matches `midnight_privacy::hash::default_blacklist_root()` exactly.
pub fn default_blacklist_root() -> [u8; 32] {
    let nodes = compute_default_bl_nodes();
    gl_digest_to_hash32(&nodes[BL_DEPTH as usize])
}

/// Build the default sparse Merkle tree nodes for an empty blacklist.
fn compute_default_bl_nodes() -> Vec<[p3_goldilocks::Goldilocks; 4]> {
    use neo_ccs::crypto::poseidon2_goldilocks as p2;
    use p3_field::PrimeCharacteristicRing;
    use p3_goldilocks::Goldilocks;

    let empty_leaf = {
        let mut input = [Goldilocks::ZERO; 1 + BL_BUCKET_SIZE * 4];
        input[0] = Goldilocks::from_u64(TAG_BL_BUCKET);
        p2::poseidon2_hash(&input)
    };

    let mut nodes: Vec<[Goldilocks; 4]> = Vec::with_capacity(BL_DEPTH as usize + 1);
    nodes.push(empty_leaf);
    for lvl in 0..BL_DEPTH {
        let prev = nodes[lvl as usize];
        let mut mt_input = [Goldilocks::ZERO; 10];
        mt_input[0] = Goldilocks::from_u64(TAG_MT_NODE_BL);
        mt_input[1] = Goldilocks::from_u64(lvl as u64);
        mt_input[2..6].copy_from_slice(&prev);
        mt_input[6..10].copy_from_slice(&prev);
        nodes.push(p2::poseidon2_hash(&mt_input));
    }
    nodes
}

/// Convert a Goldilocks digest (4 x Goldilocks) to a Hash32 (32 bytes LE).
fn gl_digest_to_hash32(digest: &[p3_goldilocks::Goldilocks; 4]) -> [u8; 32] {
    use p3_field::PrimeField64;
    let mut h = [0u8; 32];
    for (i, &elem) in digest.iter().enumerate() {
        h[i * 8..(i + 1) * 8].copy_from_slice(&elem.as_canonical_u64().to_le_bytes());
    }
    h
}

/// Derive recipient address: `H(TAG_ADDR, domain, pk_spend, pk_ivk)`.
fn derive_address_v2(domain: &[u8; 32], pk_spend: &[u8; 32], pk_ivk: &[u8; 32]) -> [u8; 32] {
    use neo_ccs::crypto::poseidon2_goldilocks as p2;
    use p3_field::PrimeCharacteristicRing;
    use p3_goldilocks::Goldilocks;

    let domain_gl = hash32_to_gl(domain);
    let pk_spend_gl = hash32_to_gl(pk_spend);
    let pk_ivk_gl = hash32_to_gl(pk_ivk);

    let mut input = [Goldilocks::ZERO; 13];
    input[0] = Goldilocks::from_u64(TAG_ADDR);
    input[1..5].copy_from_slice(&domain_gl);
    input[5..9].copy_from_slice(&pk_spend_gl);
    input[9..13].copy_from_slice(&pk_ivk_gl);
    let recipient = p2::poseidon2_hash(&input);
    gl_digest_to_hash32(&recipient)
}

/// Per-output viewer attestation data (public values the circuit will verify).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewerOutputWitness {
    /// Hash of the deterministic ciphertext (public).
    pub ct_hash: [u8; 32],
    /// MAC over (k, cm, ct_hash) (public).
    pub mac: [u8; 32],
}

/// Viewer witness data for one Level-B viewer authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewerWitness {
    /// FVK commitment (public): H("FVK_COMMIT_V1" || fvk).
    pub fvk_commitment: [u8; 32],
    /// Full viewing key (private).
    pub fvk: [u8; 32],
    /// Per-output attestation data (one per output note).
    pub per_output: Vec<ViewerOutputWitness>,
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
    /// Deny-map (blacklist) root (public).
    pub blacklist_root: [u8; 32],
    /// Blacklist non-membership proofs.
    ///
    /// Always contains 1 proof (sender). For transfers (withdraw_amount == 0),
    /// contains 2 proofs (sender + pay recipient).
    pub blacklist_proofs: Vec<BlacklistProof>,
    /// Level-B viewer attestation witnesses (empty when no viewer is configured).
    pub viewers: Vec<ViewerWitness>,
}

/// Full witness for the note-deposit circuit.
///
/// Deposits mint exactly one NOTE commitment to a derived recipient address:
/// `recipient = H(TAG_ADDR, domain, pk_spend_recipient, pk_ivk_recipient)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteDepositWitness {
    /// Domain separation tag (4 x u64 = 32 bytes).
    pub domain: [u8; 32],
    /// Public deposit amount (must be > 0 in-circuit).
    pub value: u64,
    /// Note randomness.
    pub rho: [u8; 32],
    /// Recipient spend key (private).
    pub pk_spend_recipient: [u8; 32],
    /// Recipient incoming viewing key (private).
    pub pk_ivk_recipient: [u8; 32],
    /// Public output commitment, verified in-circuit.
    pub cm_out: [u8; 32],
    /// Deny-map root (public).
    pub blacklist_root: [u8; 32],
    /// Blacklist non-membership proof for the recipient.
    pub blacklist_proof: BlacklistProof,
}

// ============================================================================
// Host
// ============================================================================

/// Host for Nightstream zkVM (Sovereign adapter).
///
/// Wraps the Nightstream `Rv64TraceWiring` builder, translating between the
/// Sovereign SDK `ZkvmHost` interface and Nightstream's RISC-V proving pipeline.
#[derive(Clone, Debug)]
pub struct NightstreamHost {
    /// Guest bytes for the circuit (currently the full RV64IM ELF).
    rom_bytes: Vec<u8>,
    /// RAM initialization values: address -> value (u32).
    ram_init: HashMap<u64, u64>,
    /// Current write offset for `add_hint` (tracks next free RAM address for inputs).
    input_offset: u64,
    /// Chunk rows (rows per trace-wiring folding step).
    chunk_rows: usize,
    /// Optional max architectural instruction bound for execution.
    max_steps: Option<usize>,
    /// Output claims: (address, expected_value).
    output_claims: Vec<(u64, u64)>,
    /// Pre-built public output bytes (bincode-serialized SpendPublic).
    ///
    /// Set by [`write_note_spend_witness`] so that after proving the host can
    /// return the correct `public_output` without needing the `SpendPublic` type.
    /// The output claims enforce that the circuit actually wrote these values.
    stored_public_output: Option<Vec<u8>>,
    /// Declares how `stored_public_output` is bound to output claims.
    public_output_format: Option<PublicOutputFormat>,
    /// Whether output binding claims should be attached to proofs.
    ///
    /// Keep this enabled by default. It can be disabled as a runtime fallback
    /// when upstream output-sumcheck limits reject a specific witness/profile.
    output_binding_enabled: bool,
}

impl NightstreamHost {
    /// Create a new NightstreamHost from guest bytes and a compatibility base address.
    pub fn new(rom_bytes: &[u8], _program_base: u64) -> Self {
        Self {
            rom_bytes: rom_bytes.to_vec(),
            ram_init: HashMap::new(),
            input_offset: DEFAULT_INPUT_ADDR,
            chunk_rows: DEFAULT_CHUNK_ROWS,
            max_steps: None,
            output_claims: Vec::new(),
            stored_public_output: None,
            public_output_format: None,
            output_binding_enabled: true,
        }
    }

    /// Enable/disable output binding claims for subsequently written witnesses.
    ///
    /// When disabled, proofs are still generated and verified, but
    /// `public_output` is not cryptographically bound via output claims.
    pub fn set_output_binding_enabled(&mut self, enabled: bool) {
        self.output_binding_enabled = enabled;
        if !enabled {
            self.output_claims.clear();
            self.public_output_format = None;
        }
    }

    /// Set the chunk rows (rows per trace-wiring folding step).
    pub fn with_chunk_rows(mut self, chunk_rows: usize) -> Self {
        self.chunk_rows = chunk_rows;
        self
    }

    /// Set an explicit max architectural instruction bound.
    ///
    /// This is forwarded to `Rv64TraceWiring::max_steps`.
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = Some(max_steps);
        self
    }

    /// Set an explicit max architectural instruction bound on an existing host.
    pub fn set_max_steps(&mut self, max_steps: usize) {
        self.max_steps = Some(max_steps);
    }

    /// Set chunk rows on an existing host.
    pub fn set_chunk_rows(&mut self, chunk_rows: usize) {
        self.chunk_rows = chunk_rows;
    }

    /// Return RAM init words as sorted `(addr, value)` pairs.
    ///
    /// Useful for simulation/debug flows that need to run the same witness
    /// outside the proving path.
    pub fn ram_init_pairs(&self) -> Vec<(u64, u32)> {
        let mut pairs: Vec<(u64, u32)> = self
            .ram_init
            .iter()
            .map(|(&addr, &value)| (addr, value as u32))
            .collect();
        pairs.sort_unstable_by_key(|(addr, _)| *addr);
        pairs
    }

    /// Add a u32 input value at the next available input address.
    ///
    /// This directly maps to the guest's `RamReader::read_u32`.
    pub fn add_u32_input(&mut self, value: u32) {
        self.ram_init.insert(self.input_offset, value as u64);
        self.input_offset += 4;
    }

    /// Add an output claim (expected value at a given RAM address).
    pub fn add_output_claim(&mut self, addr: u64, expected_value: u64) {
        // Manual output claims imply generic/raw output format unless explicitly overridden.
        self.public_output_format = None;
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

    /// Add a u32 output claim at `addr` and advance by 4 bytes.
    fn write_output_claim_u32_at(&mut self, addr: &mut u64, val: u32) {
        self.add_output_claim(*addr, val as u64);
        *addr += 4;
    }

    /// Add a u64 output claim at `addr` as two u32 words (lo, hi).
    fn write_output_claim_u64_at(&mut self, addr: &mut u64, val: u64) {
        self.write_output_claim_u32_at(addr, val as u32);
        self.write_output_claim_u32_at(addr, (val >> 32) as u32);
    }

    /// Add a digest output claim at `addr` as 4 x u64 (each split into 2 x u32).
    fn write_output_claim_digest_at(&mut self, addr: &mut u64, hash32: &[u8; 32]) {
        for i in 0..4 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&hash32[i * 8..(i + 1) * 8]);
            let val = u64::from_le_bytes(buf);
            self.write_output_claim_u64_at(addr, val);
        }
    }

    /// Build circuit-output fields directly from the witness.
    fn build_note_spend_circuit_output(witness: &NoteSpendWitness) -> CircuitOutput {
        let n_out = witness.outputs.len();
        let mut view_attestations = Vec::new();
        for viewer in &witness.viewers {
            assert_eq!(
                viewer.per_output.len(),
                n_out,
                "viewer.per_output length must equal n_out"
            );
            for (out_w, output) in viewer.per_output.iter().zip(&witness.outputs) {
                view_attestations.push(CircuitViewAttestation {
                    cm: output.cm,
                    fvk_commitment: viewer.fvk_commitment,
                    ct_hash: out_w.ct_hash,
                    mac: out_w.mac,
                });
            }
        }

        CircuitOutput {
            anchor_root: witness.anchor,
            nullifiers: witness.inputs.iter().map(|input| input.nullifier).collect(),
            withdraw_amount: witness.withdraw_amount,
            withdraw_to: witness.withdraw_to,
            output_commitments: witness.outputs.iter().map(|output| output.cm).collect(),
            blacklist_root: witness.blacklist_root,
            view_attestations,
        }
    }

    /// Populate output claims for the note-spend circuit's `RamWriter` output layout.
    fn set_note_spend_output_claims(&mut self, witness: &NoteSpendWitness) {
        self.output_claims.clear();
        let mut addr = NOTE_CIRCUIT_OUTPUT_ADDR;

        let n_in = witness.inputs.len() as u32;
        let n_out = witness.outputs.len() as u32;
        let n_viewers = witness.viewers.len() as u32;

        self.write_output_claim_digest_at(&mut addr, &witness.anchor);
        self.write_output_claim_u32_at(&mut addr, n_in);
        for input in &witness.inputs {
            self.write_output_claim_digest_at(&mut addr, &input.nullifier);
        }
        self.write_output_claim_u64_at(&mut addr, witness.withdraw_amount);
        self.write_output_claim_digest_at(&mut addr, &witness.withdraw_to);
        self.write_output_claim_u32_at(&mut addr, n_out);
        for output in &witness.outputs {
            self.write_output_claim_digest_at(&mut addr, &output.cm);
        }
        self.write_output_claim_digest_at(&mut addr, &witness.blacklist_root);
        self.write_output_claim_u32_at(&mut addr, n_viewers);

        for viewer in &witness.viewers {
            assert_eq!(
                viewer.per_output.len(),
                n_out as usize,
                "viewer.per_output length must equal n_out"
            );
            for (out_w, output) in viewer.per_output.iter().zip(&witness.outputs) {
                self.write_output_claim_digest_at(&mut addr, &output.cm);
                self.write_output_claim_digest_at(&mut addr, &viewer.fvk_commitment);
                self.write_output_claim_digest_at(&mut addr, &out_w.ct_hash);
                self.write_output_claim_digest_at(&mut addr, &out_w.mac);
            }
        }
    }

    /// Populate output claims for the note-deposit circuit's `RamWriter` output layout.
    fn set_note_deposit_output_claims(&mut self, witness: &NoteDepositWitness) {
        self.output_claims.clear();
        let mut addr = NOTE_CIRCUIT_OUTPUT_ADDR;
        let recipient = derive_address_v2(
            &witness.domain,
            &witness.pk_spend_recipient,
            &witness.pk_ivk_recipient,
        );
        self.write_output_claim_digest_at(&mut addr, &witness.domain);
        self.write_output_claim_u64_at(&mut addr, witness.value);
        self.write_output_claim_digest_at(&mut addr, &recipient);
        self.write_output_claim_digest_at(&mut addr, &witness.cm_out);
        self.write_output_claim_digest_at(&mut addr, &witness.blacklist_root);
    }

    /// Build circuit-output fields directly from a note-deposit witness.
    fn build_note_deposit_circuit_output(witness: &NoteDepositWitness) -> DepositCircuitOutput {
        let recipient = derive_address_v2(
            &witness.domain,
            &witness.pk_spend_recipient,
            &witness.pk_ivk_recipient,
        );

        DepositCircuitOutput {
            domain: witness.domain,
            value: witness.value,
            recipient,
            output_commitment: witness.cm_out,
            blacklist_root: witness.blacklist_root,
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
    /// * `public_output` - Caller-provided public output bytes (bincode SpendPublic).
    ///   The host recomputes canonical bytes from witness/output claims and stores
    ///   those for the proof package.
    pub fn write_note_spend_witness(&mut self, witness: &NoteSpendWitness, public_output: Vec<u8>) {
        self.input_offset = NOTE_CIRCUIT_INPUT_ADDR;
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

        // Blacklist root (public)
        self.write_ram_digest(&witness.blacklist_root);

        // Blacklist non-membership proofs.
        // The circuit reads: for each proof { entries[12] (digest), inv (u64), siblings[16] (digest) }
        for bl_proof in &witness.blacklist_proofs {
            for entry in &bl_proof.bucket_entries {
                self.write_ram_digest(entry);
            }
            let inv_u64 = {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bl_proof.bucket_inv[..8]);
                u64::from_le_bytes(buf)
            };
            self.write_ram_u64(inv_u64);
            for sib in &bl_proof.siblings {
                self.write_ram_digest(sib);
            }
        }

        // Viewer attestation witnesses.
        // The circuit reads: n_viewers, then per-viewer { fvk_commitment, fvk, per-output { ct_hash, mac } }
        let n_viewers = witness.viewers.len() as u32;
        self.add_u32_input(n_viewers);
        for viewer in &witness.viewers {
            self.write_ram_digest(&viewer.fvk_commitment);
            self.write_ram_digest(&viewer.fvk);
            for out_w in &viewer.per_output {
                self.write_ram_digest(&out_w.ct_hash);
                self.write_ram_digest(&out_w.mac);
            }
        }

        // Bind package public output to proof-visible output claims unless disabled.
        if self.output_binding_enabled {
            self.set_note_spend_output_claims(witness);
            self.public_output_format = Some(PublicOutputFormat::NoteSpendV1);
        } else {
            self.output_claims.clear();
            self.public_output_format = None;
        }

        let circuit_output = Self::build_note_spend_circuit_output(witness);
        let certified_public_output = spend_public_bytes_from_circuit_output(&circuit_output)
            .expect("failed to serialize certified note-spend public output");

        if public_output != certified_public_output {
            tracing::warn!(
                "Caller-provided note-spend public_output does not match certified output; using certified output bytes"
            );
        }

        self.stored_public_output = Some(certified_public_output);
    }

    /// Write the full note-deposit witness to RAM and set output claims.
    ///
    /// The note-deposit circuit has no inputs/nullifiers; it proves:
    /// 1. value > 0
    /// 2. cm_out matches NOTE(domain, value, rho, recipient, recipient)
    /// 3. recipient is not blacklisted under blacklist_root
    pub fn write_note_deposit_witness(&mut self, witness: &NoteDepositWitness) {
        self.input_offset = NOTE_CIRCUIT_INPUT_ADDR;
        // Header + note fields
        self.write_ram_digest(&witness.domain);
        self.write_ram_u64(witness.value);
        self.write_ram_digest(&witness.rho);
        self.write_ram_digest(&witness.pk_spend_recipient);
        self.write_ram_digest(&witness.pk_ivk_recipient);
        self.write_ram_digest(&witness.cm_out);
        self.write_ram_digest(&witness.blacklist_root);

        // Blacklist proof for recipient
        for entry in &witness.blacklist_proof.bucket_entries {
            self.write_ram_digest(entry);
        }
        let inv_u64 = {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&witness.blacklist_proof.bucket_inv[..8]);
            u64::from_le_bytes(buf)
        };
        self.write_ram_u64(inv_u64);
        for sib in &witness.blacklist_proof.siblings {
            self.write_ram_digest(sib);
        }

        if self.output_binding_enabled {
            self.set_note_deposit_output_claims(witness);
            // Bind package public output to proof-visible output claims.
            self.public_output_format = Some(PublicOutputFormat::NoteDepositV1);
        } else {
            self.output_claims.clear();
            self.public_output_format = None;
        }
        let circuit_output = Self::build_note_deposit_circuit_output(witness);
        self.stored_public_output = Some(
            deposit_public_bytes_from_circuit_output(&circuit_output)
                .expect("failed to serialize certified note-deposit public output"),
        );
    }

    /// Compute the SHA-256 code commitment of the guest bytes.
    pub fn compute_commitment(&self) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        hasher.update(&self.rom_bytes);
        hasher.finalize().into()
    }

    /// Build the `Rv64TraceWiring` runner from the current configuration.
    fn build_runner(&self) -> Result<Rv64TraceWiring> {
        let mut builder = Rv64TraceWiring::from_elf(&self.rom_bytes)
            .map_err(|e| anyhow::anyhow!("Nightstream RV64 guest load failed: {:?}", e))?
            .chunk_rows(self.chunk_rows);

        if let Some(max_steps) = self.max_steps {
            // Keep the trace geometry aligned with the expected execution length.
            // This avoids excess padding that can violate poseidon lane-split caps.
            builder = builder.min_trace_len(max_steps).max_steps(max_steps);
        }

        for (&addr, &value) in &self.ram_init {
            builder = builder.ram_init_u32(addr, value as u32);
        }

        for &(addr, value) in &self.output_claims {
            builder = builder.output_claim(addr, F::from_u64(value));
        }

        Ok(builder)
    }

    /// Build and prove a trace run, returning the in-memory run handle.
    ///
    /// This is useful for tests/benchmarks that want to call `run.verify()`
    /// without replaying proving from a serialized proof package.
    pub fn prove_run(&mut self) -> Result<Rv64TraceWiringRun> {
        self.prove_run_once()
    }

    /// Build the run configuration for proof packaging.
    fn build_config(&self, run: &Rv64TraceWiringRun) -> Rv64TraceWiringRunConfig {
        Rv64TraceWiringRunConfig {
            program_base: 0,
            xlen: 64,
            chunk_rows: self.chunk_rows,
            max_steps: self.max_steps,
            trace_len: Some(run.trace_len()),
            ram_init: self.ram_init.clone(),
            reg_init: HashMap::new(),
            output_claims: self.output_claims.clone(),
            public_output_format: self.public_output_format.clone(),
        }
    }
}

/// Host arguments for Nightstream: guest bytes + compatibility base address.
#[derive(Clone, Debug, Default)]
pub struct NightstreamHostArgs {
    /// Guest bytes for the circuit (currently the full RV64IM ELF).
    pub rom_bytes: Vec<u8>,
    /// Reserved for compatibility with the old ROM-based flow. Currently unused.
    pub program_base: u64,
    /// Optional max architectural instruction bound.
    pub max_steps: Option<usize>,
}

impl NightstreamHostArgs {
    /// Create new host args from guest bytes and a compatibility base address.
    pub fn new(rom_bytes: Vec<u8>, program_base: u64) -> Self {
        Self {
            rom_bytes,
            program_base,
            max_steps: None,
        }
    }
}

impl ZkvmHost for NightstreamHost {
    type Guest = NightstreamGuest;
    type HostArgs = NightstreamHostArgs;

    fn from_args(args: &Self::HostArgs) -> Self {
        let mut host = Self::new(&args.rom_bytes, args.program_base);
        if let Some(max_steps) = args.max_steps {
            host = host.with_max_steps(max_steps);
        }
        host
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
            self.ram_init.insert(self.input_offset, word as u64);
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
                "Nightstream: Generating proof with Rv64TraceWiring (chunk_rows={}, max_steps={:?})",
                self.chunk_rows,
                self.max_steps,
            );

            let prove_start = std::time::Instant::now();
            let run = self.prove_run_once()?;
            let prove_ms = prove_start.elapsed().as_millis();

            tracing::info!(
                "Nightstream: {} RISC-V instructions executed (chunk_rows={}, folding_steps={})",
                run.trace_len(),
                run.layout().t,
                run.fold_count(),
            );
            tracing::info!(
                "Nightstream CCS: {} constraints x {} variables",
                run.ccs_num_constraints(),
                run.ccs_num_variables(),
            );

            let timings = run.prove_phase_durations();
            tracing::info!(
                "Nightstream prove breakdown: \
                 setup={}ms, chunk_build_commit={}ms, fold_prove={}ms, total={}ms",
                timings.setup.as_millis(),
                timings.chunk_build_commit.as_millis(),
                timings.fold_and_prove.as_millis(),
                run.prove_duration().as_millis(),
            );

            let output_value = self.extract_output_from_run(&run)?;

            let proof_steps = run.proof().steps.len();
            let proof = encode_shard_proof_bytes(run.proof())
                .map_err(|e| anyhow::anyhow!("Nightstream proof encoding failed: {:?}", e))?;
            let steps_public = Vec::new();

            tracing::info!(
                "Nightstream: proof generated in {}ms, {} folding steps, {} step instances",
                prove_ms,
                proof_steps,
                steps_public.len(),
            );

            let package = NightstreamProofPackage {
                proof,
                steps_public,
                public_output: output_value,
                rom_bytes: self.rom_bytes.clone(),
                config: self.build_config(&run),
                pool_viewer_sig: None,
            };

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
                     proof={:.2}MB, steps_public={:.2}MB, guest={:.1}KB, \
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

            let builder = self.build_runner()?;
            let run = builder
                .prove()
                .map_err(|e| anyhow::anyhow!("Nightstream simulation prove failed: {:?}", e))?;
            let proof = encode_shard_proof_bytes(run.proof())
                .map_err(|e| anyhow::anyhow!("Nightstream proof encoding failed: {:?}", e))?;
            let steps_public = Vec::new();

            let package = NightstreamProofPackage {
                proof,
                steps_public,
                public_output: output_value,
                rom_bytes: self.rom_bytes.clone(),
                config: self.build_config(&run),
                pool_viewer_sig: None,
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
    /// Prove once with the current RAM initialization and output claims.
    ///
    /// For RV64, output claims are enforced directly by Nightstream during proving,
    /// so the successful run is already the authoritative binding source.
    fn prove_run_once(&mut self) -> Result<Rv64TraceWiringRun> {
        self.build_runner()?
            .prove()
            .map_err(|e| anyhow::anyhow!("Nightstream proving failed: {:?}", e))
    }

    /// Extract the public output for the proof package.
    ///
    /// If `stored_public_output` was set by [`write_note_spend_witness`], returns
    /// those bytes directly (the output claims enforce correctness at the ZK level).
    ///
    /// Otherwise, falls back to reconstructing raw bytes from output claims.
    fn extract_output_from_run(&self, _run: &Rv64TraceWiringRun) -> Result<Vec<u8>> {
        if !self.output_claims.is_empty() {
            if let Some(fmt) = &self.public_output_format {
                return match fmt {
                    PublicOutputFormat::NoteSpendV1 => {
                        spend_public_bytes_from_output_claims(&self.output_claims)
                    }
                    PublicOutputFormat::NoteDepositV1 => {
                        deposit_public_bytes_from_output_claims(&self.output_claims)
                    }
                };
            }
            return output_claims_to_bytes(&self.output_claims);
        }

        if let Some(ref stored) = self.stored_public_output {
            return Ok(stored.clone());
        }

        Ok(vec![])
    }

    /// Try a simulation run (no proof, just execution) to extract output.
    fn try_simulate(&self) -> Result<Vec<u8>> {
        let builder = self.build_runner()?;
        let run = builder
            .prove()
            .map_err(|e| anyhow::anyhow!("Nightstream simulation failed: {:?}", e))?;

        self.extract_output_from_run(&run)
    }
}
