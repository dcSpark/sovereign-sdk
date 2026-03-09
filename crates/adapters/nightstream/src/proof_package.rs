//! Proof package for Nightstream proofs.
//!
//! Defines `NightstreamProofPackage` and `Rv64TraceWiringRunConfig` locally so the
//! sovereign-ligero adapter is self-contained without depending on Nightstream's
//! internal bridge modules.

use crate::circuit_output::{
    deposit_public_bytes_from_output_claims, spend_public_bytes_from_output_claims,
};
use neo_ajtai::Commitment as Cmt;
use neo_ccs::{matrix::Mat, CeClaim};
use neo_fold::pi_ccs::rot_rhos_to_mats;
use neo_fold::rv64_trace_shard::Rv64TraceWiring;
use neo_fold::shard::{
    BatchedTimeProof, FoldStep, MemOrLutProof, MemSidecarProof, RlcDecProof, ShardProof, StepProof,
};
use neo_fold::{PiCcsError, PiCcsProof};
use neo_math::{F, K};
use neo_memory::output_check::OutputBindingProof;
use neo_memory::witness::StepInstanceBundle;
use p3_field::PrimeCharacteristicRing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Declares how `public_output` must be interpreted and bound to proof-visible data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublicOutputFormat {
    /// Nightstream note-spend output format written at `OUTPUT_ADDR`.
    ///
    /// Verification reconstructs raw output bytes from output-claims, parses them
    /// as note-spend circuit output, and requires `public_output` bytes to match
    /// the canonical SpendPublic wire encoding.
    NoteSpendV1,
    /// Nightstream note-deposit output format written at `OUTPUT_ADDR`.
    ///
    /// Verification reconstructs raw output bytes from output-claims, parses them
    /// as note-deposit circuit output, and requires `public_output` bytes to match
    /// the canonical note-deposit wire encoding.
    NoteDepositV1,
}

/// Configuration needed to reconstruct a run from guest bytes.
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
    /// Output claims: (address, expected_value_as_u64).
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

/// A self-contained proof package for Nightstream RV64 proofs.
///
/// Contains everything needed for an external verifier to check the proof
/// without access to the original proving session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NightstreamProofPackage {
    /// The folding proof encoded as adapter-owned binary bytes.
    ///
    /// This avoids depending on serde impls in upstream Nightstream proof types.
    pub proof: Vec<u8>,

    /// Public step instance bundles produced during proving.
    ///
    /// These carry per-step MCS commitments + public inputs, plus memory and
    /// lookup instances. The adapter keeps this field for package compatibility,
    /// but the current RV64 host path does not populate it.
    pub steps_public: Vec<StepInstanceBundle<Cmt, F, K>>,

    /// Public output bytes (bincode-serialized application output).
    pub public_output: Vec<u8>,

    /// Guest bytes (currently the full RV64IM ELF).
    pub rom_bytes: Vec<u8>,

    /// Run configuration needed to reconstruct verification context.
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

    /// Returns the number of folding steps encoded in `proof`.
    pub fn proof_step_count(&self) -> Result<usize, PiCcsError> {
        Ok(decode_shard_proof_wire(&self.proof)?.steps.len())
    }

    /// Verify this proof package by reconstructing the run from guest bytes/config and
    /// comparing canonical proof bytes.
    ///
    /// This keeps verification entirely inside the adapter without requiring serde
    /// support in upstream Nightstream proof structs.
    pub fn verify(&self) -> Result<bool, PiCcsError> {
        if self.config.xlen != 64 {
            return Err(PiCcsError::InvalidInput(format!(
                "Nightstream RV64 proof package requires xlen == 64 (got {})",
                self.config.xlen
            )));
        }

        let mut builder = Rv64TraceWiring::from_elf(&self.rom_bytes)?.chunk_rows(self.config.chunk_rows);

        if let Some(max_steps) = self.config.max_steps {
            builder = builder.max_steps(max_steps);
        }

        for (&addr, &value) in &self.config.ram_init {
            builder = builder.ram_init_u32(addr, value as u32);
        }

        for (&reg, &value) in &self.config.reg_init {
            builder = builder.reg_init_u64(reg, value);
        }

        for &(addr, value) in &self.config.output_claims {
            builder = builder.output_claim(addr, F::from_u64(value));
        }

        let run = builder.prove()?;

        // Sanity check that the regenerated proof is internally valid.
        run.verify_proof(run.proof())?;

        let regenerated = encode_shard_proof_bytes(run.proof())?;
        if regenerated != self.proof {
            return Ok(false);
        }

        if let Some(format) = &self.config.public_output_format {
            let certified = match format {
                PublicOutputFormat::NoteSpendV1 => {
                    spend_public_bytes_from_output_claims(&self.config.output_claims).map_err(
                        |e| {
                            PiCcsError::InvalidInput(format!(
                                "failed to derive certified note-spend public output from output claims: {e}"
                            ))
                        },
                    )?
                }
                PublicOutputFormat::NoteDepositV1 => {
                    deposit_public_bytes_from_output_claims(&self.config.output_claims).map_err(
                        |e| {
                            PiCcsError::InvalidInput(format!(
                                "failed to derive certified note-deposit public output from output claims: {e}"
                            ))
                        },
                    )?
                }
            };

            if certified != self.public_output {
                return Err(PiCcsError::InvalidInput(
                    "public_output does not match proof-certified output claims".to_string(),
                ));
            }
        }

        Ok(true)
    }
}

/// Encode a Nightstream shard proof into adapter-owned binary bytes.
pub(crate) fn encode_shard_proof_bytes(proof: &ShardProof) -> Result<Vec<u8>, PiCcsError> {
    let wire = ShardProofWire::from_native(proof);
    bincode::serialize(&wire)
        .map_err(|e| PiCcsError::InvalidInput(format!("proof wire serialization failed: {e}")))
}

fn decode_shard_proof_wire(bytes: &[u8]) -> Result<ShardProofWire, PiCcsError> {
    bincode::deserialize(bytes)
        .map_err(|e| PiCcsError::InvalidInput(format!("proof wire deserialization failed: {e}")))
}

fn default_chunk_rows() -> usize {
    1 << 16
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimeSumcheckWire {
    claimed_sum: K,
    round_polys: Vec<Vec<K>>,
    r_time: Vec<K>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimePointOpeningWire {
    point: Vec<K>,
    col_ids: Vec<usize>,
    evals: Vec<K>,
    source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimeOpeningProofWire {
    point: Vec<K>,
    col_ids: Vec<usize>,
    evals: Vec<K>,
    digit_evals: Vec<Vec<K>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpeningClaimEntryWire {
    point: Vec<K>,
    col_ids: Vec<usize>,
    source: String,
    domain: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpeningClaimManifestWire {
    entries: Vec<OpeningClaimEntryWire>,
    digest: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpeningReductionGroupWire {
    point: Vec<K>,
    domain: String,
    claim_indices: Vec<usize>,
    group_digest: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpeningReductionProofWire {
    groups: Vec<OpeningReductionGroupWire>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpeningUnificationProofWire {
    claimed_sum: K,
    round_polys: Vec<Vec<K>>,
    r_unify: Vec<K>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JointOpeningGroupProofWire {
    point: Vec<K>,
    domain: String,
    claim_indices: Vec<usize>,
    group_digest: [u8; 32],
    joint_claim_digits: Vec<K>,
    joint_claim: K,
    joint_commitment: Cmt,
    opening_ccs_proof: Option<PiCcsProof>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JointOpeningLaneProofWire {
    claim_kind: String,
    groups: Vec<JointOpeningGroupProofWire>,
    unified_fold: Option<JointOpeningGroupProofWire>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FoldingLanesWire {
    main_children: usize,
    val_children: usize,
    wb_children: usize,
    wp_children: usize,
    stage8_children: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ShoutAddrPreGroupProofWire {
    ell_addr: u32,
    active_lanes: Vec<u32>,
    round_polys: Vec<Vec<Vec<K>>>,
    r_addr: Vec<K>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ShoutAddrPreProofWire {
    claimed_sums: Vec<K>,
    groups: Vec<ShoutAddrPreGroupProofWire>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum MemOrLutProofWire {
    Twist(neo_memory::twist::TwistProof<K>),
    Shout(neo_memory::shout::ShoutProof<K>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MemSidecarProofWire {
    val_me_claims: Vec<CeClaim<Cmt, F, K>>,
    wb_me_claims: Vec<CeClaim<Cmt, F, K>>,
    wp_me_claims: Vec<CeClaim<Cmt, F, K>>,
    poseidon_cycle_me_claims: Vec<CeClaim<Cmt, F, K>>,
    poseidon_local_me_claims: Vec<CeClaim<Cmt, F, K>>,
    shout_addr_pre: ShoutAddrPreProofWire,
    proofs: Vec<MemOrLutProofWire>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BatchedTimeProofWire {
    claimed_sums: Vec<K>,
    degree_bounds: Vec<usize>,
    labels: Vec<Vec<u8>>,
    round_polys: Vec<Vec<Vec<K>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RlcDecProofWire {
    rlc_rhos: Vec<Mat<F>>,
    rlc_parent: CeClaim<Cmt, F, K>,
    dec_children: Vec<CeClaim<Cmt, F, K>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FoldStepWire {
    ccs_out: Vec<CeClaim<Cmt, F, K>>,
    ccs_proof: PiCcsProof,
    rlc_rhos: Vec<Mat<F>>,
    rlc_parent: CeClaim<Cmt, F, K>,
    dec_children: Vec<CeClaim<Cmt, F, K>>,
    cpu_sumcheck: TimeSumcheckWire,
    shift_sumcheck: TimeSumcheckWire,
    time_cpu_commitments: Vec<Cmt>,
    time_mem_commitments: Vec<Cmt>,
    time_t: usize,
    time_declared_len: usize,
    time_col_ids: Vec<usize>,
    memory_time_proofs: Vec<Vec<u8>>,
    openings: Vec<TimePointOpeningWire>,
    opening_proofs: Vec<TimeOpeningProofWire>,
    opening_manifest: OpeningClaimManifestWire,
    opening_reduction: OpeningReductionProofWire,
    opening_unification: OpeningUnificationProofWire,
    joint_opening_lane: JointOpeningLaneProofWire,
    folding_lanes: FoldingLanesWire,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StepProofWire {
    fold: FoldStepWire,
    mem: MemSidecarProofWire,
    batched_time: BatchedTimeProofWire,
    poseidon_local_time: Option<BatchedTimeProofWire>,
    poseidon_cycle_fold: Vec<RlcDecProofWire>,
    poseidon_local_fold: Vec<RlcDecProofWire>,
    val_fold: Vec<RlcDecProofWire>,
    wb_fold: Vec<RlcDecProofWire>,
    wp_fold: Vec<RlcDecProofWire>,
    compressed_substeps: Option<Vec<StepProofWire>>,
    stage8_fold: Vec<RlcDecProofWire>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ShardSegmentMetaWire {
    kind: String,
    public_steps: usize,
    proof_steps: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ShardProofWire {
    steps: Vec<StepProofWire>,
    output_proof: Option<OutputBindingProof>,
    segment_meta: Option<Vec<ShardSegmentMetaWire>>,
}

impl ShardProofWire {
    fn from_native(proof: &ShardProof) -> Self {
        Self {
            steps: proof.steps.iter().map(StepProofWire::from_native).collect(),
            output_proof: proof.output_proof.clone(),
            segment_meta: proof.segment_meta.as_ref().map(|meta| {
                meta.iter()
                    .map(|m| ShardSegmentMetaWire {
                        kind: format!("{:?}", m.kind),
                        public_steps: m.public_steps,
                        proof_steps: m.proof_steps,
                    })
                    .collect()
            }),
        }
    }
}

impl StepProofWire {
    fn from_native(step: &StepProof) -> Self {
        Self {
            fold: FoldStepWire::from_native(&step.fold),
            mem: MemSidecarProofWire::from_native(&step.mem),
            batched_time: BatchedTimeProofWire::from_native(&step.batched_time),
            poseidon_local_time: step
                .poseidon_local_time
                .as_ref()
                .map(BatchedTimeProofWire::from_native),
            poseidon_cycle_fold: step
                .poseidon_cycle_fold
                .iter()
                .map(RlcDecProofWire::from_native)
                .collect(),
            poseidon_local_fold: step
                .poseidon_local_fold
                .iter()
                .map(RlcDecProofWire::from_native)
                .collect(),
            val_fold: step
                .val_fold
                .iter()
                .map(RlcDecProofWire::from_native)
                .collect(),
            wb_fold: step
                .wb_fold
                .iter()
                .map(RlcDecProofWire::from_native)
                .collect(),
            wp_fold: step
                .wp_fold
                .iter()
                .map(RlcDecProofWire::from_native)
                .collect(),
            compressed_substeps: step.compressed_substeps.as_ref().map(|sub| {
                sub.iter()
                    .map(StepProofWire::from_native)
                    .collect::<Vec<_>>()
            }),
            stage8_fold: step
                .stage8_fold
                .iter()
                .map(RlcDecProofWire::from_native)
                .collect(),
        }
    }
}

impl FoldStepWire {
    fn from_native(step: &FoldStep) -> Self {
        Self {
            ccs_out: step.ccs_out.clone(),
            ccs_proof: step.ccs_proof.clone(),
            rlc_rhos: rot_rhos_to_mats(&step.rlc_rhos),
            rlc_parent: step.rlc_parent.clone(),
            dec_children: step.dec_children.clone(),
            cpu_sumcheck: TimeSumcheckWire {
                claimed_sum: step.cpu_sumcheck.claimed_sum,
                round_polys: step.cpu_sumcheck.round_polys.clone(),
                r_time: step.cpu_sumcheck.r_time.clone(),
            },
            shift_sumcheck: TimeSumcheckWire {
                claimed_sum: step.shift_sumcheck.claimed_sum,
                round_polys: step.shift_sumcheck.round_polys.clone(),
                r_time: step.shift_sumcheck.r_time.clone(),
            },
            time_cpu_commitments: step.time_cpu_commitments.clone(),
            time_mem_commitments: step.time_mem_commitments.clone(),
            time_t: step.time_t,
            time_declared_len: step.time_declared_len,
            time_col_ids: step.time_col_ids.clone(),
            memory_time_proofs: step
                .memory_time_proofs
                .iter()
                .map(|label| (*label).to_vec())
                .collect(),
            openings: step
                .openings
                .iter()
                .map(|o| TimePointOpeningWire {
                    point: o.point.clone(),
                    col_ids: o.col_ids.clone(),
                    evals: o.evals.clone(),
                    source: format!("{:?}", o.source),
                })
                .collect(),
            opening_proofs: step
                .opening_proofs
                .iter()
                .map(|o| TimeOpeningProofWire {
                    point: o.point.clone(),
                    col_ids: o.col_ids.clone(),
                    evals: o.evals.clone(),
                    digit_evals: o.digit_evals.clone(),
                })
                .collect(),
            opening_manifest: OpeningClaimManifestWire {
                entries: step
                    .opening_manifest
                    .entries
                    .iter()
                    .map(|entry| OpeningClaimEntryWire {
                        point: entry.point.clone(),
                        col_ids: entry.col_ids.clone(),
                        source: format!("{:?}", entry.source),
                        domain: format!("{:?}", entry.domain),
                    })
                    .collect(),
                digest: step.opening_manifest.digest,
            },
            opening_reduction: OpeningReductionProofWire {
                groups: step
                    .opening_reduction
                    .groups
                    .iter()
                    .map(|group| OpeningReductionGroupWire {
                        point: group.point.clone(),
                        domain: format!("{:?}", group.domain),
                        claim_indices: group.claim_indices.clone(),
                        group_digest: group.group_digest,
                    })
                    .collect(),
            },
            opening_unification: OpeningUnificationProofWire {
                claimed_sum: step.opening_unification.claimed_sum,
                round_polys: step.opening_unification.round_polys.clone(),
                r_unify: step.opening_unification.r_unify.clone(),
            },
            joint_opening_lane: JointOpeningLaneProofWire {
                claim_kind: format!("{:?}", step.joint_opening_lane.claim_kind),
                groups: step
                    .joint_opening_lane
                    .groups
                    .iter()
                    .map(|group| JointOpeningGroupProofWire {
                        point: group.point.clone(),
                        domain: format!("{:?}", group.domain),
                        claim_indices: group.claim_indices.clone(),
                        group_digest: group.group_digest,
                        joint_claim_digits: group.joint_claim_digits.clone(),
                        joint_claim: group.joint_claim,
                        joint_commitment: group.joint_commitment.clone(),
                        opening_ccs_proof: group.opening_ccs_proof.clone(),
                    })
                    .collect(),
                unified_fold: step.joint_opening_lane.unified_fold.as_ref().map(|group| {
                    JointOpeningGroupProofWire {
                        point: group.point.clone(),
                        domain: format!("{:?}", group.domain),
                        claim_indices: group.claim_indices.clone(),
                        group_digest: group.group_digest,
                        joint_claim_digits: group.joint_claim_digits.clone(),
                        joint_claim: group.joint_claim,
                        joint_commitment: group.joint_commitment.clone(),
                        opening_ccs_proof: group.opening_ccs_proof.clone(),
                    }
                }),
            },
            folding_lanes: FoldingLanesWire {
                main_children: step.folding_lanes.main_children,
                val_children: step.folding_lanes.val_children,
                wb_children: step.folding_lanes.wb_children,
                wp_children: step.folding_lanes.wp_children,
                stage8_children: step.folding_lanes.stage8_children,
            },
        }
    }
}

impl MemSidecarProofWire {
    fn from_native(mem: &MemSidecarProof<Cmt, F, K>) -> Self {
        Self {
            val_me_claims: mem.val_me_claims.clone(),
            wb_me_claims: mem.wb_me_claims.clone(),
            wp_me_claims: mem.wp_me_claims.clone(),
            poseidon_cycle_me_claims: mem.poseidon_cycle_me_claims.clone(),
            poseidon_local_me_claims: mem.poseidon_local_me_claims.clone(),
            shout_addr_pre: ShoutAddrPreProofWire {
                claimed_sums: mem.shout_addr_pre.claimed_sums.clone(),
                groups: mem
                    .shout_addr_pre
                    .groups
                    .iter()
                    .map(|g| ShoutAddrPreGroupProofWire {
                        ell_addr: g.ell_addr,
                        active_lanes: g.active_lanes.clone(),
                        round_polys: g.round_polys.clone(),
                        r_addr: g.r_addr.clone(),
                    })
                    .collect(),
            },
            proofs: mem
                .proofs
                .iter()
                .map(|proof| match proof {
                    MemOrLutProof::Twist(t) => MemOrLutProofWire::Twist(t.clone()),
                    MemOrLutProof::Shout(s) => MemOrLutProofWire::Shout(s.clone()),
                })
                .collect(),
        }
    }
}

impl BatchedTimeProofWire {
    fn from_native(proof: &BatchedTimeProof) -> Self {
        Self {
            claimed_sums: proof.claimed_sums.clone(),
            degree_bounds: proof.degree_bounds.clone(),
            labels: proof.labels.iter().map(|label| (*label).to_vec()).collect(),
            round_polys: proof.round_polys.clone(),
        }
    }
}

impl RlcDecProofWire {
    fn from_native(proof: &RlcDecProof) -> Self {
        Self {
            rlc_rhos: rot_rhos_to_mats(&proof.rlc_rhos),
            rlc_parent: proof.rlc_parent.clone(),
            dec_children: proof.dec_children.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_wire_roundtrip_rejects_invalid_bytes() {
        let bytes = vec![1u8, 2, 3, 4];
        let err = decode_shard_proof_wire(&bytes).expect_err("invalid proof wire should fail");
        match err {
            PiCcsError::InvalidInput(_) => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
