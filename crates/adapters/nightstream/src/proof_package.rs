//! Proof package for Nightstream proofs.
//!
//! The package is self-contained for proofs emitted by this adapter's `/prove`
//! path: it carries the shard proof plus the verifier-side public context
//! needed to run the lightweight shard verifier directly, without replaying
//! `Rv64TraceWiring::prove()`.

use crate::circuit_output::{
    deposit_public_bytes_from_output_claims, digest32_from_output_claims,
    spend_public_binding_digest_from_public_bytes,
};
use neo_ajtai::{s_lincomb, s_mul, Commitment as Cmt};
use neo_ccs::{matrix::Mat, CcsStructure, CeClaim};
use neo_fold::output_binding::OutputBindingConfig;
use neo_fold::pi_ccs::FoldingMode;
use neo_fold::shard::{
    fold_shard_verify, fold_shard_verify_with_output_binding,
    fold_shard_verify_with_output_binding_and_step_linking, fold_shard_verify_with_step_linking,
    CommitMixers, ShardFoldOutputs, ShardProof, StepLinkingConfig,
};
use neo_fold::PiCcsError;
use neo_math::ring::{cf_inv, Rq as RqEl};
use neo_math::{D, F, K};
use neo_memory::output_check::ProgramIO;
use neo_memory::witness::StepInstanceBundle;
use neo_memory::{AffineWordAddressRemap, RiscvGuestMemoryLayout, RiscvProofProfileConfig};
use neo_params::NeoParams;
use neo_transcript::{Poseidon2Transcript, Transcript};
use p3_field::PrimeCharacteristicRing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::marker::PhantomData;

/// Declares how `public_output` must be interpreted and bound to proof-visible data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublicOutputFormat {
    /// Nightstream note-spend output format written at `OUTPUT_ADDR`.
    ///
    /// Verification recomputes a digest of the canonical SpendPublic bytes and
    /// requires it to match the proof-certified digest claims.
    NoteSpendV1,
    /// Nightstream note-deposit output format written at `OUTPUT_ADDR`.
    ///
    /// Verification reconstructs raw output bytes from output-claims, parses them
    /// as note-deposit circuit output, and requires `public_output` bytes to match
    /// the canonical note-deposit wire encoding.
    NoteDepositV1,
}

/// Configuration needed to reconstruct the guest-visible statement.
///
/// The lightweight verifier uses [`NightstreamVerifierContext`] for the actual
/// shard verification. This config remains in the package so the application
/// output binding can be rechecked against the guest-visible output claims.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rv64TraceWiringRunConfig {
    /// Reserved for compatibility with the old ROM-based flow. Always `0`.
    pub program_base: u64,
    /// Word size (must be 64 for RV64IM guests).
    pub xlen: usize,
    /// Rows per trace step used during proving (`chunk_rows` in builder).
    #[serde(default = "default_chunk_rows")]
    pub chunk_rows: usize,
    /// Optional max step bound used during proving.
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// Executed architectural instruction count (`trace_len`) reported by the run.
    #[serde(default)]
    pub trace_len: Option<usize>,
    /// Initial RAM values: address -> value.
    pub ram_init: HashMap<u64, u64>,
    /// Initial register values: register index -> value.
    pub reg_init: HashMap<u64, u64>,
    /// Guest-visible output claims: `(guest_addr, expected_value_as_u64)`.
    pub output_claims: Vec<(u64, u64)>,
    /// Optional format used to bind `public_output` to proof-visible outputs.
    #[serde(default)]
    pub public_output_format: Option<PublicOutputFormat>,
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

/// Output-binding context needed by the lightweight packaged verifier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputBindingConfigWire {
    /// Number of logical address bits in the bound memory space.
    pub num_bits: usize,
    /// Claimed logical program outputs.
    pub program_io: ProgramIO<F>,
    /// Logical memory identifier the proof bound against.
    pub mem_id: u32,
}

impl OutputBindingConfigWire {
    fn to_native(
        &self,
        steps: &[StepInstanceBundle<Cmt, F, K>],
    ) -> Result<OutputBindingConfig, PiCcsError> {
        let mem_idx = steps
            .first()
            .and_then(|step| {
                step.mem_insts
                    .iter()
                    .position(|inst| inst.mem_id == self.mem_id)
            })
            .ok_or_else(|| {
                PiCcsError::InvalidInput(format!(
                    "missing mem_id={} in packaged step instances for output binding",
                    self.mem_id
                ))
            })?;

        for (step_idx, step) in steps.iter().enumerate().skip(1) {
            match step
                .mem_insts
                .iter()
                .position(|inst| inst.mem_id == self.mem_id)
            {
                Some(idx) if idx == mem_idx => {}
                Some(idx) => {
                    return Err(PiCcsError::InvalidInput(format!(
                        "mem_id={} moved from mem_idx={} to mem_idx={} at step {}",
                        self.mem_id, mem_idx, idx, step_idx
                    )));
                }
                None => {
                    return Err(PiCcsError::InvalidInput(format!(
                        "missing mem_id={} in packaged step {} for output binding",
                        self.mem_id, step_idx
                    )));
                }
            }
        }

        Ok(OutputBindingConfig::new(self.num_bits, self.program_io.clone()).with_mem_idx(mem_idx))
    }
}

/// Verifier-only public context needed to verify a packaged proof directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NightstreamVerifierContext {
    /// The prepared CCS relation used by the shard verifier.
    pub ccs: CcsStructure<F>,
    /// The public Neo parameter set used during proving.
    pub params: NeoParams,
    /// Public step instances produced during proving.
    pub steps_public: Vec<StepInstanceBundle<Cmt, F, K>>,
    /// Optional output-binding configuration for proofs that bind final memory state.
    #[serde(default)]
    pub output_binding: Option<OutputBindingConfigWire>,
    /// Step-linking equalities `(prev_col, next_col)` applied across step boundaries.
    #[serde(default)]
    pub step_linking_pairs: Vec<(usize, usize)>,
    /// Optional verifier-visible RISC-V proof profile metadata.
    #[serde(default)]
    pub proof_profile: Option<RiscvProofProfileConfig>,
    /// Optional deterministic guest-memory layout bound to the proof.
    #[serde(default)]
    pub memory_layout: Option<RiscvGuestMemoryLayout>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NightstreamVerifierContextWire {
    ccs: Vec<u8>,
    params: Vec<u8>,
    steps_public: Vec<StepInstanceBundleBytes>,
    #[serde(default)]
    output_binding: Option<OutputBindingConfigWire>,
    #[serde(default)]
    step_linking_pairs: Vec<(usize, usize)>,
    #[serde(default)]
    proof_profile: Option<RiscvProofProfileConfig>,
    #[serde(default)]
    memory_layout: Option<RiscvGuestMemoryLayout>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StepInstanceBundleBytes {
    mcs_inst: Vec<u8>,
    lut_insts: Vec<u8>,
    mem_insts: Vec<MemInstanceWire>,
    time_columns: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MemInstanceWire {
    mem_id: u32,
    comms: Vec<u8>,
    k: usize,
    d: usize,
    n_side: usize,
    steps: usize,
    lanes: usize,
    ell: usize,
    init: Vec<u8>,
    init_digest: Option<[u8; 32]>,
    guest_addr_remap: Option<AffineWordAddressRemap>,
}

impl MemInstanceWire {
    fn from_native(
        inst: &neo_memory::witness::MemInstance<Cmt, F>,
        step_idx: usize,
        mem_idx: usize,
    ) -> Result<Self, PiCcsError> {
        let comms = bincode::serialize(&inst.comms).map_err(|e| {
            PiCcsError::InvalidInput(format!(
                "verifier context step[{step_idx}] memory[{mem_idx}] commitments serialization failed: {e}"
            ))
        })?;
        let init = bincode::serialize(&inst.init).map_err(|e| {
            PiCcsError::InvalidInput(format!(
                "verifier context step[{step_idx}] memory[{mem_idx}] init serialization failed: {e}"
            ))
        })?;

        Ok(Self {
            mem_id: inst.mem_id,
            comms,
            k: inst.k,
            d: inst.d,
            n_side: inst.n_side,
            steps: inst.steps,
            lanes: inst.lanes,
            ell: inst.ell,
            init,
            init_digest: inst.init_digest,
            guest_addr_remap: inst.guest_addr_remap.clone(),
        })
    }

    fn to_native(
        &self,
        step_idx: usize,
        mem_idx: usize,
    ) -> Result<neo_memory::witness::MemInstance<Cmt, F>, PiCcsError> {
        Ok(neo_memory::witness::MemInstance {
            mem_id: self.mem_id,
            comms: bincode::deserialize(&self.comms).map_err(|e| {
                PiCcsError::InvalidInput(format!(
                    "verifier context step[{step_idx}] memory[{mem_idx}] commitments deserialization failed: {e}"
                ))
            })?,
            k: self.k,
            d: self.d,
            n_side: self.n_side,
            steps: self.steps,
            lanes: self.lanes,
            ell: self.ell,
            init: bincode::deserialize(&self.init).map_err(|e| {
                PiCcsError::InvalidInput(format!(
                    "verifier context step[{step_idx}] memory[{mem_idx}] init deserialization failed: {e}"
                ))
            })?,
            init_digest: self.init_digest,
            guest_addr_remap: self.guest_addr_remap.clone(),
        })
    }
}

/// A self-contained proof package for Nightstream RV64 proofs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NightstreamProofPackage {
    /// The native shard proof emitted by Nightstream.
    pub proof: ShardProof,
    /// Public verifier context packaged by the `/prove` path.
    ///
    /// Stored as adapter-owned bytes because upstream `MemInstance` does not
    /// bincode-roundtrip cleanly when embedded directly.
    pub verifier_context: Vec<u8>,
    /// Public output bytes (bincode-serialized application output).
    pub public_output: Vec<u8>,
    /// Guest bytes (currently the full RV64IM ELF).
    pub rom_bytes: Vec<u8>,
    /// Guest-visible statement/configuration metadata.
    pub config: Rv64TraceWiringRunConfig,
    /// Optional pool-operator signature over the viewer FVK commitment.
    #[serde(default)]
    pub pool_viewer_sig: Option<PoolViewerSig>,
}

impl NightstreamProofPackage {
    /// Serialize this package to bytes (using bincode).
    pub fn to_bytes(&self) -> Result<Vec<u8>, PiCcsError> {
        bincode::serialize(self).map_err(|e| {
            PiCcsError::InvalidInput(format!("proof package serialization failed: {e}"))
        })
    }

    /// Deserialize a package from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PiCcsError> {
        bincode::deserialize(bytes).map_err(|e| {
            PiCcsError::InvalidInput(format!("proof package deserialization failed: {e}"))
        })
    }

    /// Returns the number of folding steps encoded in the packaged proof.
    pub fn proof_step_count(&self) -> Result<usize, PiCcsError> {
        Ok(self.proof.steps.len())
    }

    /// Verify this proof package directly against the packaged verifier context.
    pub fn verify(&self) -> Result<bool, PiCcsError> {
        if self.config.xlen != 64 {
            return Err(PiCcsError::InvalidInput(format!(
                "Nightstream RV64 proof package requires xlen == 64 (got {})",
                self.config.xlen
            )));
        }

        self.verify_lightweight()?;
        self.verify_public_output_binding()?;
        Ok(true)
    }

    fn verify_lightweight(&self) -> Result<(), PiCcsError> {
        let verifier_context = decode_verifier_context(&self.verifier_context)?;
        validate_proof_metadata(&verifier_context, &self.proof)?;

        let steps_public = &verifier_context.steps_public;
        if steps_public.is_empty() {
            return Err(PiCcsError::InvalidInput(
                "packaged verifier context is missing step instances".into(),
            ));
        }

        let mut transcript = Poseidon2Transcript::new(b"neo.fold/session");
        let seed_me: &[CeClaim<Cmt, F, K>] = &[];
        let mode = FoldingMode::Optimized;
        let mixers = default_mixers();

        let outputs: ShardFoldOutputs<Cmt, F, K> = match verifier_context.output_binding.as_ref() {
            Some(ob_wire) => {
                let ob_cfg = ob_wire.to_native(steps_public)?;
                if steps_public.len() > 1 {
                    let step_linking = required_step_linking(&verifier_context.step_linking_pairs)?;
                    fold_shard_verify_with_output_binding_and_step_linking(
                        mode,
                        &mut transcript,
                        &verifier_context.params,
                        &verifier_context.ccs,
                        steps_public,
                        seed_me,
                        &self.proof,
                        mixers,
                        &ob_cfg,
                        &step_linking,
                    )?
                } else {
                    fold_shard_verify_with_output_binding(
                        mode,
                        &mut transcript,
                        &verifier_context.params,
                        &verifier_context.ccs,
                        steps_public,
                        seed_me,
                        &self.proof,
                        mixers,
                        &ob_cfg,
                    )?
                }
            }
            None => {
                if steps_public.len() > 1 {
                    let step_linking = required_step_linking(&verifier_context.step_linking_pairs)?;
                    fold_shard_verify_with_step_linking(
                        mode,
                        &mut transcript,
                        &verifier_context.params,
                        &verifier_context.ccs,
                        steps_public,
                        seed_me,
                        &self.proof,
                        mixers,
                        &step_linking,
                    )?
                } else {
                    fold_shard_verify(
                        mode,
                        &mut transcript,
                        &verifier_context.params,
                        &verifier_context.ccs,
                        steps_public,
                        seed_me,
                        &self.proof,
                        mixers,
                    )?
                }
            }
        };

        validate_shard_outputs(steps_public, &outputs)
    }

    fn verify_public_output_binding(&self) -> Result<(), PiCcsError> {
        if let Some(format) = &self.config.public_output_format {
            match format {
                PublicOutputFormat::NoteSpendV1 => {
                    let certified =
                        spend_public_binding_digest_from_public_bytes(&self.public_output)
                            .map_err(|e| {
                                PiCcsError::InvalidInput(format!(
                            "failed to derive note-spend binding digest from public_output: {e}"
                        ))
                            })?;
                    let claimed =
                        digest32_from_output_claims(&self.config.output_claims).map_err(|e| {
                            PiCcsError::InvalidInput(format!(
                                "failed to derive note-spend binding digest from output claims: {e}"
                            ))
                        })?;
                    if certified != claimed {
                        return Err(PiCcsError::InvalidInput(
                            "public_output does not match proof-certified note-spend digest"
                                .to_string(),
                        ));
                    }
                }
                PublicOutputFormat::NoteDepositV1 => {
                    let certified =
                        deposit_public_bytes_from_output_claims(&self.config.output_claims)
                            .map_err(|e| {
                                PiCcsError::InvalidInput(format!(
                                    "failed to derive certified note-deposit public output from output claims: {e}"
                                ))
                            })?;
                    if certified != self.public_output {
                        return Err(PiCcsError::InvalidInput(
                            "public_output does not match proof-certified output claims"
                                .to_string(),
                        ));
                    }
                }
            }
        }

        Ok(())
    }
}

fn default_chunk_rows() -> usize {
    1 << 16
}

pub(crate) fn encode_verifier_context(
    ctx: &NightstreamVerifierContext,
) -> Result<Vec<u8>, PiCcsError> {
    let steps_public = ctx
        .steps_public
        .iter()
        .enumerate()
        .map(|(step_idx, step)| {
            Ok(StepInstanceBundleBytes {
                mcs_inst: bincode::serialize(&step.mcs_inst).map_err(|e| {
                    PiCcsError::InvalidInput(format!(
                        "verifier context step[{step_idx}] MCS serialization failed: {e}"
                    ))
                })?,
                lut_insts: bincode::serialize(&step.lut_insts).map_err(|e| {
                    PiCcsError::InvalidInput(format!(
                        "verifier context step[{step_idx}] LUT serialization failed: {e}"
                    ))
                })?,
                mem_insts: step
                    .mem_insts
                    .iter()
                    .enumerate()
                    .map(|(mem_idx, inst)| MemInstanceWire::from_native(inst, step_idx, mem_idx))
                    .collect::<Result<Vec<_>, _>>()?,
                time_columns: bincode::serialize(&step.time_columns).map_err(|e| {
                    PiCcsError::InvalidInput(format!(
                        "verifier context step[{step_idx}] time-column serialization failed: {e}"
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, PiCcsError>>()?;

    let wire = NightstreamVerifierContextWire {
        ccs: bincode::serialize(&ctx.ccs).map_err(|e| {
            PiCcsError::InvalidInput(format!("verifier context CCS serialization failed: {e}"))
        })?,
        params: bincode::serialize(&ctx.params).map_err(|e| {
            PiCcsError::InvalidInput(format!("verifier context params serialization failed: {e}"))
        })?,
        steps_public,
        output_binding: ctx.output_binding.clone(),
        step_linking_pairs: ctx.step_linking_pairs.clone(),
        proof_profile: ctx.proof_profile.clone(),
        memory_layout: ctx.memory_layout.clone(),
    };

    bincode::serialize(&wire).map_err(|e| {
        PiCcsError::InvalidInput(format!("verifier context wire serialization failed: {e}"))
    })
}

pub(crate) fn decode_verifier_context(
    bytes: &[u8],
) -> Result<NightstreamVerifierContext, PiCcsError> {
    let wire: NightstreamVerifierContextWire = bincode::deserialize(bytes).map_err(|e| {
        PiCcsError::InvalidInput(format!("verifier context wire deserialization failed: {e}"))
    })?;

    Ok(NightstreamVerifierContext {
        ccs: bincode::deserialize(&wire.ccs).map_err(|e| {
            PiCcsError::InvalidInput(format!("verifier context CCS deserialization failed: {e}"))
        })?,
        params: bincode::deserialize(&wire.params).map_err(|e| {
            PiCcsError::InvalidInput(format!(
                "verifier context params deserialization failed: {e}"
            ))
        })?,
        steps_public: wire
            .steps_public
            .into_iter()
            .enumerate()
            .map(|(step_idx, step)| {
                let mcs_inst = bincode::deserialize(&step.mcs_inst).map_err(|e| {
                    PiCcsError::InvalidInput(format!(
                        "verifier context step[{step_idx}] MCS deserialization failed: {e}"
                    ))
                })?;
                let lut_insts = bincode::deserialize(&step.lut_insts).map_err(|e| {
                    PiCcsError::InvalidInput(format!(
                        "verifier context step[{step_idx}] LUT deserialization failed: {e}"
                    ))
                })?;
                let mem_insts = step
                    .mem_insts
                    .into_iter()
                    .enumerate()
                    .map(|(mem_idx, inst)| inst.to_native(step_idx, mem_idx))
                    .collect::<Result<Vec<_>, _>>()?;
                let time_columns = bincode::deserialize(&step.time_columns).map_err(|e| {
                    PiCcsError::InvalidInput(format!(
                        "verifier context step[{step_idx}] time-column deserialization failed: {e}"
                    ))
                })?;
                Ok(StepInstanceBundle {
                    mcs_inst,
                    lut_insts,
                    mem_insts,
                    time_columns,
                    _phantom: PhantomData,
                })
            })
            .collect::<Result<Vec<_>, PiCcsError>>()?,
        output_binding: wire.output_binding,
        step_linking_pairs: wire.step_linking_pairs,
        proof_profile: wire.proof_profile,
        memory_layout: wire.memory_layout,
    })
}

fn required_step_linking(pairs: &[(usize, usize)]) -> Result<StepLinkingConfig, PiCcsError> {
    if pairs.is_empty() {
        return Err(PiCcsError::InvalidInput(
            "multi-step packaged verification requires step_linking_pairs".into(),
        ));
    }
    Ok(StepLinkingConfig::new(pairs.to_vec()))
}

fn validate_proof_metadata(
    ctx: &NightstreamVerifierContext,
    proof: &ShardProof,
) -> Result<(), PiCcsError> {
    if let Some(expected) = &ctx.proof_profile {
        let actual = proof.riscv_profile.as_ref().ok_or_else(|| {
            PiCcsError::InvalidInput(
                "packaged verifier context expected riscv_profile, but proof omitted it".into(),
            )
        })?;
        if actual != expected {
            return Err(PiCcsError::ProtocolError(format!(
                "RISC-V proof profile mismatch: packaged={expected:?} proof={actual:?}"
            )));
        }
    }

    if let Some(expected) = &ctx.memory_layout {
        let actual = proof.riscv_memory_layout.as_ref().ok_or_else(|| {
            PiCcsError::InvalidInput(
                "packaged verifier context expected riscv_memory_layout, but proof omitted it"
                    .into(),
            )
        })?;
        if actual != expected {
            return Err(PiCcsError::ProtocolError(format!(
                "RISC-V memory layout mismatch: packaged={expected:?} proof={actual:?}"
            )));
        }
    }

    Ok(())
}

fn validate_shard_outputs(
    steps_public: &[StepInstanceBundle<Cmt, F, K>],
    outputs: &ShardFoldOutputs<Cmt, F, K>,
) -> Result<(), PiCcsError> {
    let has_twist_or_shout = steps_public
        .iter()
        .any(|step| !step.mem_insts.is_empty() || !step.lut_insts.is_empty());
    if !has_twist_or_shout && !outputs.obligations.val.is_empty() {
        return Err(PiCcsError::ProtocolError(
            "CCS-only packaged verification produced unexpected val-lane obligations".into(),
        ));
    }
    Ok(())
}

type Mixers = CommitMixers<fn(&[Mat<F>], &[Cmt]) -> Cmt, fn(&[Cmt], u32) -> Cmt>;

fn rot_matrix_to_rq(mat: &Mat<F>) -> RqEl {
    debug_assert_eq!(mat.rows(), D);
    debug_assert_eq!(mat.cols(), D);

    let mut coeffs = [F::ZERO; D];
    for i in 0..D {
        coeffs[i] = mat[(i, 0)];
    }
    cf_inv(coeffs)
}

fn default_mixers() -> Mixers {
    fn mix_rhos_commits(rhos: &[Mat<F>], cs: &[Cmt]) -> Cmt {
        debug_assert!(!cs.is_empty(), "mix_rhos_commits: empty commitments");
        let rq_els: Vec<RqEl> = rhos.iter().map(rot_matrix_to_rq).collect();
        s_lincomb(&rq_els, cs).expect("s_lincomb should succeed for packaged verification")
    }

    fn combine_b_pows(cs: &[Cmt], b: u32) -> Cmt {
        debug_assert!(!cs.is_empty(), "combine_b_pows: empty commitments");
        let mut acc = cs[0].clone();
        let mut pow = F::from_u64(b as u64);
        for c in cs.iter().skip(1) {
            let rq_pow = RqEl::from_field_scalar(pow);
            let term = s_mul(&rq_pow, c);
            acc.add_inplace(&term);
            pow *= F::from_u64(b as u64);
        }
        acc
    }

    CommitMixers {
        mix_rhos_commits,
        combine_b_pows,
    }
}
