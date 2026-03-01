//! Structural equivalence tests for the Goldilocks note-spend circuit.
//!
//! Contains a native Rust reference implementation of the note-spend logic
//! using `neo_ccs::crypto::poseidon2_goldilocks` directly, plus tests that
//! verify the reference produces structurally correct outputs matching the
//! Ligero circuit's logic.

use neo_ccs::crypto::poseidon2_goldilocks::poseidon2_hash;
use p3_field::{PrimeCharacteristicRing, PrimeField64};
use p3_goldilocks::Goldilocks;

// ============================================================================
// Types
// ============================================================================

/// Goldilocks digest: 4 field elements (32 bytes total).
type GlDigest = [Goldilocks; 4];

const ZERO_DIGEST: GlDigest = [Goldilocks::ZERO; 4];

fn digest_from_u64s(vals: [u64; 4]) -> GlDigest {
    vals.map(Goldilocks::from_u64)
}

fn _digest_to_u64s(d: &GlDigest) -> [u64; 4] {
    [
        d[0].as_canonical_u64(),
        d[1].as_canonical_u64(),
        d[2].as_canonical_u64(),
        d[3].as_canonical_u64(),
    ]
}

// ============================================================================
// Domain separation tags (must match the RISC-V circuit)
// ============================================================================

const TAG_MT_NODE: u64 = 1;
const TAG_NOTE: u64 = 2;
const TAG_PRF_NF: u64 = 3;
const TAG_PK: u64 = 4;
const TAG_ADDR: u64 = 5;
const TAG_NFKEY: u64 = 6;

// ============================================================================
// Reference implementation
// ============================================================================

/// Derive pk_spend = H(TAG_PK, spend_sk).
fn reference_derive_pk_spend(spend_sk: &GlDigest) -> GlDigest {
    let mut input = [Goldilocks::ZERO; 5];
    input[0] = Goldilocks::from_u64(TAG_PK);
    input[1..5].copy_from_slice(spend_sk);
    poseidon2_hash(&input)
}

/// Derive nf_key = H(TAG_NFKEY, domain, spend_sk).
fn reference_derive_nf_key(domain: &GlDigest, spend_sk: &GlDigest) -> GlDigest {
    let mut input = [Goldilocks::ZERO; 9];
    input[0] = Goldilocks::from_u64(TAG_NFKEY);
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(spend_sk);
    poseidon2_hash(&input)
}

/// Derive address = H(TAG_ADDR, domain, pk_spend, pk_ivk).
fn reference_derive_address(domain: &GlDigest, pk_spend: &GlDigest, pk_ivk: &GlDigest) -> GlDigest {
    let mut input = [Goldilocks::ZERO; 13];
    input[0] = Goldilocks::from_u64(TAG_ADDR);
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(pk_spend);
    input[9..13].copy_from_slice(pk_ivk);
    poseidon2_hash(&input)
}

/// Compute note commitment = H(TAG_NOTE, domain, value, rho, recipient, sender_id).
fn reference_note_commitment(
    domain: &GlDigest,
    value: Goldilocks,
    rho: &GlDigest,
    recipient: &GlDigest,
    sender_id: &GlDigest,
) -> GlDigest {
    let mut input = [Goldilocks::ZERO; 18];
    input[0] = Goldilocks::from_u64(TAG_NOTE);
    input[1..5].copy_from_slice(domain);
    input[5] = value;
    input[6..10].copy_from_slice(rho);
    input[10..14].copy_from_slice(recipient);
    input[14..18].copy_from_slice(sender_id);
    poseidon2_hash(&input)
}

/// Derive nullifier = H(TAG_PRF_NF, domain, nf_key, rho).
fn reference_nullifier(domain: &GlDigest, nf_key: &GlDigest, rho: &GlDigest) -> GlDigest {
    let mut input = [Goldilocks::ZERO; 13];
    input[0] = Goldilocks::from_u64(TAG_PRF_NF);
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(nf_key);
    input[9..13].copy_from_slice(rho);
    poseidon2_hash(&input)
}

/// Compute Merkle tree node = H(TAG_MT_NODE, level, left, right).
fn reference_mt_node(level: u64, left: &GlDigest, right: &GlDigest) -> GlDigest {
    let mut input = [Goldilocks::ZERO; 10];
    input[0] = Goldilocks::from_u64(TAG_MT_NODE);
    input[1] = Goldilocks::from_u64(level);
    input[2..6].copy_from_slice(left);
    input[6..10].copy_from_slice(right);
    poseidon2_hash(&input)
}

/// Compute Merkle root from a leaf, position, and siblings.
fn reference_merkle_root(leaf: &GlDigest, pos: u32, siblings: &[GlDigest]) -> GlDigest {
    let mut cur = *leaf;
    let mut p = pos;

    for (lvl, sib) in siblings.iter().enumerate() {
        let bit = p & 1;
        let (left, right) = if bit == 0 { (&cur, sib) } else { (sib, &cur) };
        cur = reference_mt_node(lvl as u64, left, right);
        p >>= 1;
    }

    cur
}

/// Build a Merkle tree from leaves, returning (root, all_siblings_per_leaf).
///
/// Pads to next power of 2 with zero digests.
fn build_merkle_tree(leaves: &[GlDigest]) -> (GlDigest, Vec<Vec<GlDigest>>) {
    let n = leaves.len().next_power_of_two();
    let depth = n.trailing_zeros() as usize;

    // Pad leaves
    let mut layer: Vec<GlDigest> = leaves.to_vec();
    layer.resize(n, ZERO_DIGEST);

    let mut layers = vec![layer.clone()];

    for lvl in 0..depth {
        let prev = &layers[lvl];
        let mut next = Vec::with_capacity(prev.len() / 2);
        for pair in prev.chunks(2) {
            next.push(reference_mt_node(lvl as u64, &pair[0], &pair[1]));
        }
        layers.push(next);
    }

    let root = layers[depth][0];

    // Extract siblings for each original leaf
    let mut all_siblings = Vec::new();
    for leaf_idx in 0..leaves.len() {
        let mut siblings = Vec::new();
        let mut idx = leaf_idx;
        for lvl in 0..depth {
            let sibling_idx = idx ^ 1;
            siblings.push(layers[lvl][sibling_idx]);
            idx /= 2;
        }
        all_siblings.push(siblings);
    }

    (root, all_siblings)
}

/// Public output of the note-spend circuit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpendPublic {
    anchor: GlDigest,
    nullifiers: Vec<GlDigest>,
    withdraw_amount: u64,
    withdraw_to: GlDigest,
    output_commitments: Vec<GlDigest>,
    blacklist_root: GlDigest,
}

/// Full reference note-spend computation.
fn reference_note_spend(
    domain: &GlDigest,
    spend_sk: &GlDigest,
    pk_ivk_owner: &GlDigest,
    anchor: &GlDigest,
    depth: usize,
    inputs: &[(u64, GlDigest, GlDigest, u32, Vec<GlDigest>)], // (value, rho, sender_id_in, pos, siblings)
    withdraw_amount: u64,
    withdraw_to: &GlDigest,
    outputs: &[(u64, GlDigest, GlDigest, GlDigest)], // (value, rho, pk_spend_out, pk_ivk_out)
    blacklist_root: &GlDigest,
) -> SpendPublic {
    // Derive owner identity
    let pk_spend_owner = reference_derive_pk_spend(spend_sk);
    let nf_key = reference_derive_nf_key(domain, spend_sk);
    let recipient_owner = reference_derive_address(domain, &pk_spend_owner, pk_ivk_owner);
    let sender_id = recipient_owner;

    // Process inputs
    let mut sum_in = Goldilocks::ZERO;
    let mut nullifiers = Vec::new();

    for (value_in, rho_in, sender_id_in, pos, siblings) in inputs {
        sum_in += Goldilocks::from_u64(*value_in);

        let cm = reference_note_commitment(
            domain,
            Goldilocks::from_u64(*value_in),
            rho_in,
            &recipient_owner,
            sender_id_in,
        );
        let root = reference_merkle_root(&cm, *pos, &siblings[..depth]);
        assert_eq!(root, *anchor, "Merkle root mismatch for input");

        let nf = reference_nullifier(domain, &nf_key, rho_in);
        nullifiers.push(nf);
    }

    // Process outputs
    let mut out_sum = Goldilocks::ZERO;
    let mut output_commitments = Vec::new();

    for (value_out, rho_out, pk_spend_out, pk_ivk_out) in outputs {
        out_sum += Goldilocks::from_u64(*value_out);
        let rcp = reference_derive_address(domain, pk_spend_out, pk_ivk_out);
        let cm = reference_note_commitment(
            domain,
            Goldilocks::from_u64(*value_out),
            rho_out,
            &rcp,
            &sender_id,
        );
        output_commitments.push(cm);
    }

    // Balance check
    let rhs = Goldilocks::from_u64(withdraw_amount) + out_sum;
    assert_eq!(sum_in, rhs, "Balance check failed");

    SpendPublic {
        anchor: *anchor,
        nullifiers,
        withdraw_amount,
        withdraw_to: *withdraw_to,
        output_commitments,
        blacklist_root: *blacklist_root,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn test_poseidon2_domain_separation() {
    // Each domain tag should produce a distinct hash for the same payload.
    let payload = digest_from_u64s([42, 43, 44, 45]);

    let h1 = {
        let mut input = [Goldilocks::ZERO; 5];
        input[0] = Goldilocks::from_u64(TAG_PK);
        input[1..5].copy_from_slice(&payload);
        poseidon2_hash(&input)
    };

    let h2 = {
        let mut input = [Goldilocks::ZERO; 5];
        input[0] = Goldilocks::from_u64(TAG_NOTE);
        input[1..5].copy_from_slice(&payload);
        poseidon2_hash(&input)
    };

    let h3 = {
        let mut input = [Goldilocks::ZERO; 5];
        input[0] = Goldilocks::from_u64(TAG_PRF_NF);
        input[1..5].copy_from_slice(&payload);
        poseidon2_hash(&input)
    };

    assert_ne!(
        h1, h2,
        "TAG_PK and TAG_NOTE should produce different hashes"
    );
    assert_ne!(
        h1, h3,
        "TAG_PK and TAG_PRF_NF should produce different hashes"
    );
    assert_ne!(
        h2, h3,
        "TAG_NOTE and TAG_PRF_NF should produce different hashes"
    );
}

#[test]
fn test_poseidon2_deterministic() {
    let input = [
        Goldilocks::from_u64(1),
        Goldilocks::from_u64(2),
        Goldilocks::from_u64(3),
    ];
    let h1 = poseidon2_hash(&input);
    let h2 = poseidon2_hash(&input);
    assert_eq!(h1, h2, "Poseidon2 must be deterministic");
}

#[test]
fn test_key_derivation() {
    let domain = digest_from_u64s([100, 200, 300, 400]);
    let spend_sk = digest_from_u64s([1, 2, 3, 4]);
    let pk_ivk_owner = digest_from_u64s([5, 6, 7, 8]);

    let pk_spend = reference_derive_pk_spend(&spend_sk);
    let nf_key = reference_derive_nf_key(&domain, &spend_sk);
    let addr = reference_derive_address(&domain, &pk_spend, &pk_ivk_owner);

    // All outputs should be non-zero and distinct
    assert_ne!(pk_spend, ZERO_DIGEST);
    assert_ne!(nf_key, ZERO_DIGEST);
    assert_ne!(addr, ZERO_DIGEST);
    assert_ne!(pk_spend, nf_key);
    assert_ne!(pk_spend, addr);
    assert_ne!(nf_key, addr);
}

#[test]
fn test_nullifier_derivation() {
    let domain = digest_from_u64s([100, 200, 300, 400]);
    let spend_sk = digest_from_u64s([1, 2, 3, 4]);
    let nf_key = reference_derive_nf_key(&domain, &spend_sk);

    let rho1 = digest_from_u64s([10, 20, 30, 40]);
    let rho2 = digest_from_u64s([50, 60, 70, 80]);

    let nf1 = reference_nullifier(&domain, &nf_key, &rho1);
    let nf2 = reference_nullifier(&domain, &nf_key, &rho2);

    // Same key + different rho = different nullifiers
    assert_ne!(nf1, nf2, "Different rhos must produce different nullifiers");

    // Deterministic
    let nf1b = reference_nullifier(&domain, &nf_key, &rho1);
    assert_eq!(nf1, nf1b, "Nullifier derivation must be deterministic");
}

#[test]
fn test_merkle_path_verification() {
    // Build a small Merkle tree (4 leaves, depth 2)
    let leaves: Vec<GlDigest> = (0..4)
        .map(|i| digest_from_u64s([i * 10 + 1, i * 10 + 2, i * 10 + 3, i * 10 + 4]))
        .collect();

    let (root, all_siblings) = build_merkle_tree(&leaves);

    // Verify each leaf's path
    for (i, leaf) in leaves.iter().enumerate() {
        let computed_root = reference_merkle_root(leaf, i as u32, &all_siblings[i]);
        assert_eq!(
            computed_root, root,
            "Merkle path verification failed for leaf {i}"
        );
    }

    // Wrong position should give wrong root
    let wrong_root = reference_merkle_root(&leaves[0], 1, &all_siblings[0]);
    assert_ne!(wrong_root, root, "Wrong position should give wrong root");
}

#[test]
fn test_balance_check() {
    let a = Goldilocks::from_u64(100);
    let b = Goldilocks::from_u64(60);
    let c = Goldilocks::from_u64(40);

    // 100 == 60 + 40
    assert_eq!(a, b + c);

    // Goldilocks field overflow wraps correctly
    let p_minus_1 = Goldilocks::from_u64(0xFFFF_FFFF_0000_0000);
    let one = Goldilocks::from_u64(1);
    assert_eq!(p_minus_1 + one, Goldilocks::ZERO);
}

#[test]
fn test_note_commitment_and_spend_public() {
    let domain = digest_from_u64s([100, 200, 300, 400]);
    let spend_sk = digest_from_u64s([1, 2, 3, 4]);
    let pk_ivk_owner = digest_from_u64s([5, 6, 7, 8]);
    let blacklist_root = digest_from_u64s([99, 88, 77, 66]);

    // Derive owner keys
    let pk_spend_owner = reference_derive_pk_spend(&spend_sk);
    let recipient_owner = reference_derive_address(&domain, &pk_spend_owner, &pk_ivk_owner);

    // Create two input notes
    let input_values = [50u64, 30u64];
    let input_rhos = [
        digest_from_u64s([10, 11, 12, 13]),
        digest_from_u64s([20, 21, 22, 23]),
    ];
    let sender_id_ins = [recipient_owner, recipient_owner];

    // Compute commitments
    let input_cms: Vec<GlDigest> = (0..2)
        .map(|i| {
            reference_note_commitment(
                &domain,
                Goldilocks::from_u64(input_values[i]),
                &input_rhos[i],
                &recipient_owner,
                &sender_id_ins[i],
            )
        })
        .collect();

    // Build Merkle tree with the input commitments as leaves (pad to 4)
    let mut tree_leaves = input_cms.clone();
    tree_leaves.push(ZERO_DIGEST);
    tree_leaves.push(ZERO_DIGEST);
    let (anchor, all_siblings) = build_merkle_tree(&tree_leaves);

    // Create one output note (transfer: 80 = 50 + 30, withdraw = 0)
    let output_value = 80u64;
    let output_rho = digest_from_u64s([30, 31, 32, 33]);
    let pk_spend_out = digest_from_u64s([40, 41, 42, 43]);
    let pk_ivk_out = digest_from_u64s([50, 51, 52, 53]);

    // Build inputs for reference
    let inputs: Vec<(u64, GlDigest, GlDigest, u32, Vec<GlDigest>)> = (0..2)
        .map(|i| {
            (
                input_values[i],
                input_rhos[i],
                sender_id_ins[i],
                i as u32,
                all_siblings[i].clone(),
            )
        })
        .collect();

    let outputs = vec![(output_value, output_rho, pk_spend_out, pk_ivk_out)];
    let withdraw_to = ZERO_DIGEST;

    let spend_public = reference_note_spend(
        &domain,
        &spend_sk,
        &pk_ivk_owner,
        &anchor,
        2, // depth
        &inputs,
        0, // withdraw_amount
        &withdraw_to,
        &outputs,
        &blacklist_root,
    );

    // Structural checks
    assert_eq!(spend_public.anchor, anchor);
    assert_eq!(spend_public.nullifiers.len(), 2);
    assert_eq!(spend_public.output_commitments.len(), 1);
    assert_eq!(spend_public.withdraw_amount, 0);
    assert_eq!(spend_public.blacklist_root, blacklist_root);

    // Nullifiers should be non-zero and distinct
    assert_ne!(spend_public.nullifiers[0], ZERO_DIGEST);
    assert_ne!(spend_public.nullifiers[1], ZERO_DIGEST);
    assert_ne!(
        spend_public.nullifiers[0], spend_public.nullifiers[1],
        "Nullifiers must be distinct"
    );

    // Output commitment should be non-zero
    assert_ne!(spend_public.output_commitments[0], ZERO_DIGEST);
}

#[test]
fn test_withdraw_scenario() {
    let domain = digest_from_u64s([100, 200, 300, 400]);
    let spend_sk = digest_from_u64s([1, 2, 3, 4]);
    let pk_ivk_owner = digest_from_u64s([5, 6, 7, 8]);
    let blacklist_root = digest_from_u64s([99, 88, 77, 66]);
    let withdraw_to = digest_from_u64s([111, 222, 333, 444]);

    let pk_spend_owner = reference_derive_pk_spend(&spend_sk);
    let recipient_owner = reference_derive_address(&domain, &pk_spend_owner, &pk_ivk_owner);

    // Single input note worth 100
    let input_value = 100u64;
    let input_rho = digest_from_u64s([10, 11, 12, 13]);

    let input_cm = reference_note_commitment(
        &domain,
        Goldilocks::from_u64(input_value),
        &input_rho,
        &recipient_owner,
        &recipient_owner,
    );

    let tree_leaves = vec![input_cm, ZERO_DIGEST];
    let (anchor, all_siblings) = build_merkle_tree(&tree_leaves);

    let inputs = vec![(
        input_value,
        input_rho,
        recipient_owner,
        0u32,
        all_siblings[0].clone(),
    )];

    // Withdraw 100, no outputs
    let spend_public = reference_note_spend(
        &domain,
        &spend_sk,
        &pk_ivk_owner,
        &anchor,
        1, // depth
        &inputs,
        100, // withdraw_amount
        &withdraw_to,
        &[], // no outputs
        &blacklist_root,
    );

    assert_eq!(spend_public.anchor, anchor);
    assert_eq!(spend_public.nullifiers.len(), 1);
    assert_eq!(spend_public.output_commitments.len(), 0);
    assert_eq!(spend_public.withdraw_amount, 100);
    assert_eq!(spend_public.withdraw_to, withdraw_to);
}

#[test]
fn test_reference_consistency_multiple_runs() {
    // Run the same scenario twice and verify identical outputs.
    let domain = digest_from_u64s([100, 200, 300, 400]);
    let spend_sk = digest_from_u64s([1, 2, 3, 4]);
    let pk_ivk_owner = digest_from_u64s([5, 6, 7, 8]);
    let blacklist_root = ZERO_DIGEST;

    let pk_spend_owner = reference_derive_pk_spend(&spend_sk);
    let recipient_owner = reference_derive_address(&domain, &pk_spend_owner, &pk_ivk_owner);

    let input_rho = digest_from_u64s([10, 11, 12, 13]);
    let input_cm = reference_note_commitment(
        &domain,
        Goldilocks::from_u64(50),
        &input_rho,
        &recipient_owner,
        &recipient_owner,
    );
    let tree_leaves = vec![input_cm, ZERO_DIGEST];
    let (anchor, all_siblings) = build_merkle_tree(&tree_leaves);

    let inputs = vec![(
        50u64,
        input_rho,
        recipient_owner,
        0u32,
        all_siblings[0].clone(),
    )];
    let output_rho = digest_from_u64s([30, 31, 32, 33]);
    let pk_spend_out = digest_from_u64s([40, 41, 42, 43]);
    let pk_ivk_out = digest_from_u64s([50, 51, 52, 53]);
    let outputs = vec![(50u64, output_rho, pk_spend_out, pk_ivk_out)];

    let sp1 = reference_note_spend(
        &domain,
        &spend_sk,
        &pk_ivk_owner,
        &anchor,
        1,
        &inputs,
        0,
        &ZERO_DIGEST,
        &outputs,
        &blacklist_root,
    );
    let sp2 = reference_note_spend(
        &domain,
        &spend_sk,
        &pk_ivk_owner,
        &anchor,
        1,
        &inputs,
        0,
        &ZERO_DIGEST,
        &outputs,
        &blacklist_root,
    );

    assert_eq!(sp1, sp2, "Reference implementation must be deterministic");
}
