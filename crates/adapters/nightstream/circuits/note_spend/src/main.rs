/*
 * RISC-V Guest Program: Note Spend (Goldilocks / Poseidon2)
 *
 * Implements the join-split spend verifier from the Midnight privacy protocol,
 * ported from the Ligero BN254-based circuit to use Goldilocks field arithmetic
 * and Poseidon2-Goldilocks hashing via the Nightstream ECALL precompile.
 *
 * The circuit proves:
 *   1. Ownership: all inputs belong to the same spend key
 *   2. Merkle membership: each input commitment is in the state tree
 *   3. Nullifiers: correctly derived and match public values
 *   4. Output commitments: correctly formed
 *   5. Balance: sum(inputs) == withdraw_amount + sum(outputs)
 *   6. Enforce product: values non-zero, rhos distinct
 */

#![no_std]
#![no_main]

use nightstream_sdk::goldilocks::*;
use nightstream_sdk::poseidon2::poseidon2_hash;

// --- Constants ---

const MAX_INS: usize = 4;
const MAX_OUTS: usize = 2;
const MAX_DEPTH: usize = 63;

// Domain separation tags (Goldilocks constants).
const TAG_MT_NODE: u64 = 1;
const TAG_NOTE: u64 = 2;
const TAG_PRF_NF: u64 = 3;
const TAG_PK: u64 = 4;
const TAG_ADDR: u64 = 5;
const TAG_NFKEY: u64 = 6;

// --- RAM I/O helpers ---

const INPUT_ADDR: u32 = 0x104;
const OUTPUT_ADDR: u32 = 0x100;

struct RamReader {
    addr: u32,
}

impl RamReader {
    fn new(addr: u32) -> Self {
        Self { addr }
    }

    fn read_u32(&mut self) -> u32 {
        let val = unsafe { core::ptr::read_volatile(self.addr as *const u32) };
        self.addr += 4;
        val
    }

    fn read_u64(&mut self) -> u64 {
        let lo = self.read_u32() as u64;
        let hi = self.read_u32() as u64;
        lo | (hi << 32)
    }

    fn read_digest(&mut self) -> GlDigest {
        [self.read_u64(), self.read_u64(), self.read_u64(), self.read_u64()]
    }
}

struct RamWriter {
    addr: u32,
}

impl RamWriter {
    fn new(addr: u32) -> Self {
        Self { addr }
    }

    fn write_u32(&mut self, val: u32) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u32, val) };
        self.addr += 4;
    }

    fn write_u64(&mut self, val: u64) {
        self.write_u32(val as u32);
        self.write_u32((val >> 32) as u32);
    }

    fn write_digest(&mut self, d: &GlDigest) {
        for &elem in d {
            self.write_u64(elem);
        }
    }
}

// --- Cryptographic primitives ---

/// Hash domain: pk_spend = H(TAG_PK, spend_sk)
fn derive_pk_spend(spend_sk: &GlDigest) -> GlDigest {
    let mut input = [0u64; 5];
    input[0] = TAG_PK;
    input[1..5].copy_from_slice(spend_sk);
    poseidon2_hash(&input)
}

/// Hash domain: nf_key = H(TAG_NFKEY, domain, spend_sk)
fn derive_nf_key(domain: &GlDigest, spend_sk: &GlDigest) -> GlDigest {
    let mut input = [0u64; 9];
    input[0] = TAG_NFKEY;
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(spend_sk);
    poseidon2_hash(&input)
}

/// Hash domain: address = H(TAG_ADDR, domain, pk_spend, pk_ivk)
fn derive_address(domain: &GlDigest, pk_spend: &GlDigest, pk_ivk: &GlDigest) -> GlDigest {
    let mut input = [0u64; 13];
    input[0] = TAG_ADDR;
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(pk_spend);
    input[9..13].copy_from_slice(pk_ivk);
    poseidon2_hash(&input)
}

/// Hash domain: note commitment = H(TAG_NOTE, domain, value, rho, recipient, sender_id)
fn note_commitment(
    domain: &GlDigest,
    value: u64,
    rho: &GlDigest,
    recipient: &GlDigest,
    sender_id: &GlDigest,
) -> GlDigest {
    let mut input = [0u64; 18];
    input[0] = TAG_NOTE;
    input[1..5].copy_from_slice(domain);
    input[5] = value;
    input[6..10].copy_from_slice(rho);
    input[10..14].copy_from_slice(recipient);
    input[14..18].copy_from_slice(sender_id);
    poseidon2_hash(&input)
}

/// Hash domain: nullifier = H(TAG_PRF_NF, domain, nf_key, rho)
fn derive_nullifier(domain: &GlDigest, nf_key: &GlDigest, rho: &GlDigest) -> GlDigest {
    let mut input = [0u64; 13];
    input[0] = TAG_PRF_NF;
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(nf_key);
    input[9..13].copy_from_slice(rho);
    poseidon2_hash(&input)
}

/// Merkle tree node: H(TAG_MT_NODE, level, left, right)
fn mt_node(level: u64, left: &GlDigest, right: &GlDigest) -> GlDigest {
    let mut input = [0u64; 10];
    input[0] = TAG_MT_NODE;
    input[1] = level;
    input[2..6].copy_from_slice(left);
    input[6..10].copy_from_slice(right);
    poseidon2_hash(&input)
}

/// Verify a Merkle path and return the computed root.
///
/// Uses arithmetic MUX (no branching) for constant-time path selection:
///   delta[i] = bit * (sib[i] - cur[i])
///   left[i]  = cur[i] + delta[i]
///   right[i] = sib[i] - delta[i]
fn merkle_root(
    leaf: &GlDigest,
    pos: u32,
    reader: &mut RamReader,
    depth: u32,
) -> GlDigest {
    let mut cur = *leaf;
    let mut p = pos;

    for lvl in 0..depth {
        let sib = reader.read_digest();
        let bit = (p & 1) as u64;

        let mut left = [0u64; 4];
        let mut right = [0u64; 4];
        for i in 0..4 {
            let delta = gl_mul(bit, gl_sub(sib[i], cur[i]));
            left[i] = gl_add(cur[i], delta);
            right[i] = gl_sub(sib[i], delta);
        }

        cur = mt_node(lvl as u64, &left, &right);
        p >>= 1;
    }

    cur
}

/// Compute the "enforce product" contribution for a digest difference.
/// Returns product of (a[i] - b[i]) for i in 0..4, accumulated into `acc`.
fn enforce_prod_digest_diff(acc: u64, a: &GlDigest, b: &GlDigest) -> u64 {
    let mut result = acc;
    for i in 0..4 {
        let diff = gl_sub(a[i], b[i]);
        result = gl_mul(result, diff);
    }
    result
}

// --- Main circuit ---

#[nightstream_sdk::provable]
fn note_spend() -> ! {
    let mut r = RamReader::new(INPUT_ADDR);
    let mut w = RamWriter::new(OUTPUT_ADDR);

    // 1. Parse header
    let domain = r.read_digest();
    let spend_sk = r.read_digest();
    let pk_ivk_owner = r.read_digest();
    let depth = r.read_u32();
    let anchor = r.read_digest();
    let n_in = r.read_u32();

    // 2. Derive owner identity
    let pk_spend_owner = derive_pk_spend(&spend_sk);
    let nf_key = derive_nf_key(&domain, &spend_sk);
    let recipient_owner = derive_address(&domain, &pk_spend_owner, &pk_ivk_owner);
    let sender_id = recipient_owner;

    // 3. Process inputs: commitments, Merkle paths, nullifiers
    let mut sum_in: u64 = GL_ZERO;
    let mut enforce_prod: u64 = GL_ONE;
    let mut input_rhos: [GlDigest; MAX_INS] = [ZERO_DIGEST; MAX_INS];
    let mut input_cms: [GlDigest; MAX_INS] = [ZERO_DIGEST; MAX_INS];

    for i in 0..n_in as usize {
        let value_in = r.read_u64();
        let rho_in = r.read_digest();
        let sender_id_in = r.read_digest();
        let pos = r.read_u32();

        sum_in = gl_add(sum_in, value_in);
        enforce_prod = gl_mul(enforce_prod, value_in);

        let cm = note_commitment(&domain, value_in, &rho_in, &recipient_owner, &sender_id_in);
        input_cms[i] = cm;
        input_rhos[i] = rho_in;

        let root = merkle_root(&cm, pos, &mut r, depth);

        // Assert root == anchor
        assert!(digest_eq(&root, &anchor));
    }

    // Read public nullifiers and verify
    let mut nullifiers: [GlDigest; MAX_INS] = [ZERO_DIGEST; MAX_INS];
    for i in 0..n_in as usize {
        let nullifier_pub = r.read_digest();
        let nf = derive_nullifier(&domain, &nf_key, &input_rhos[i]);
        assert!(digest_eq(&nf, &nullifier_pub));
        nullifiers[i] = nullifier_pub;
    }

    // 4. Read withdraw info
    let withdraw_amount = r.read_u64();
    let withdraw_to = r.read_digest();
    let n_out = r.read_u32();

    // 5. Process outputs: commitments
    let mut out_sum: u64 = GL_ZERO;
    let mut output_rhos: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];
    let mut output_cms: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];
    let mut output_rcps: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];

    for j in 0..n_out as usize {
        let value_out = r.read_u64();
        let rho_out = r.read_digest();
        let pk_spend_out = r.read_digest();
        let pk_ivk_out = r.read_digest();

        out_sum = gl_add(out_sum, value_out);
        enforce_prod = gl_mul(enforce_prod, value_out);

        let rcp = derive_address(&domain, &pk_spend_out, &pk_ivk_out);
        let cm = note_commitment(&domain, value_out, &rho_out, &rcp, &sender_id);

        output_rhos[j] = rho_out;
        output_cms[j] = cm;
        output_rcps[j] = rcp;
    }

    // Read public output commitments and verify
    let mut cm_outs_pub: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];
    for j in 0..n_out as usize {
        let cm_pub = r.read_digest();
        assert!(digest_eq(&output_cms[j], &cm_pub));
        cm_outs_pub[j] = cm_pub;
    }

    // 6. Balance check: sum_in == withdraw_amount + out_sum
    let rhs = gl_add(withdraw_amount, out_sum);
    assert!(sum_in == rhs);

    // 7. Change output rules
    if withdraw_amount > 0 && n_out == 1 {
        assert!(digest_eq(&output_rcps[0], &sender_id));
    }
    if withdraw_amount == 0 && n_out == 2 {
        assert!(digest_eq(&output_rcps[1], &sender_id));
    }

    // 8. Enforce product: values non-zero, output rhos distinct from input rhos
    for j in 0..n_out as usize {
        for i in 0..n_in as usize {
            enforce_prod = enforce_prod_digest_diff(enforce_prod, &output_rhos[j], &input_rhos[i]);
        }
    }
    if n_out == 2 {
        enforce_prod = enforce_prod_digest_diff(enforce_prod, &output_rhos[0], &output_rhos[1]);
    }

    let inv_enforce = r.read_u64();
    let check = gl_mul(enforce_prod, inv_enforce);
    assert!(check == GL_ONE);

    // 9. Read blacklist root (public, binding only -- full check deferred)
    let blacklist_root = r.read_digest();

    // 10. Write SpendPublic output
    w.write_digest(&anchor);
    w.write_u32(n_in);
    for i in 0..n_in as usize {
        w.write_digest(&nullifiers[i]);
    }
    w.write_u64(withdraw_amount);
    w.write_digest(&withdraw_to);
    w.write_u32(n_out);
    for j in 0..n_out as usize {
        w.write_digest(&cm_outs_pub[j]);
    }
    w.write_digest(&blacklist_root);

    nightstream_sdk::halt();
}
