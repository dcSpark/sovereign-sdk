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
 *   3. Nullifiers: correctly derived, match public values, pairwise distinct
 *   4. Shape validation: n_out / withdraw_amount / withdraw_to consistency
 *   5. Output commitments: correctly formed
 *   6. Balance: sum(inputs) == withdraw_amount + sum(outputs)
 *   7. Enforce product: values non-zero, rhos distinct
 *   8. Blacklist non-membership: sender (and pay recipient) not in deny-map
 *   9. Viewer attestations (Level-B): FVK commitment, ct_hash, MAC binding
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
const TAG_BL_BUCKET: u64 = 7;

// Blacklist (deny-map) tree parameters.
const BL_DEPTH: u32 = 16;
const BL_BUCKET_SIZE: usize = 12;

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

// --- Blacklist (deny-map) primitives ---

/// Compute the bucket leaf hash: H(TAG_BL_BUCKET, entries[0..4], ..., entries[11][0..4])
/// Input: 1 (tag) + 12 * 4 (entries) = 49 Goldilocks field elements.
fn bl_bucket_leaf(entries: &[GlDigest; BL_BUCKET_SIZE]) -> GlDigest {
    let mut input = [0u64; 1 + BL_BUCKET_SIZE * 4];
    input[0] = TAG_BL_BUCKET;
    for (i, entry) in entries.iter().enumerate() {
        let off = 1 + i * 4;
        input[off..off + 4].copy_from_slice(entry);
    }
    poseidon2_hash(&input)
}

/// Extract bucket position (low BL_DEPTH bits) from a GlDigest identifier.
/// id[0] holds the least-significant 64 bits in LE encoding.
fn bl_bucket_pos(id: &GlDigest) -> u32 {
    (id[0] as u32) & ((1u32 << BL_DEPTH) - 1)
}

/// Assert that `id` is NOT in the blacklist rooted at `blacklist_root`.
///
/// Reads from RAM:
///   - BL_BUCKET_SIZE bucket entries (GlDigest each)
///   - 1 inverse witness (u64)
///   - BL_DEPTH Merkle siblings (GlDigest each)
///
/// Proves non-membership via: product(id - entry_i) * inv == 1.
/// Then verifies the bucket leaf hashes to `blacklist_root` via Merkle path.
fn assert_not_blacklisted(
    id: &GlDigest,
    blacklist_root: &GlDigest,
    reader: &mut RamReader,
) {
    let mut entries = [ZERO_DIGEST; BL_BUCKET_SIZE];
    for e in entries.iter_mut() {
        *e = reader.read_digest();
    }
    let bucket_inv = reader.read_u64();

    let mut prod: u64 = GL_ONE;
    for entry in &entries {
        prod = enforce_prod_digest_diff(prod, id, entry);
    }
    assert!(gl_mul(prod, bucket_inv) == GL_ONE);

    let leaf = bl_bucket_leaf(&entries);
    let pos = bl_bucket_pos(id);
    let root = merkle_root(&leaf, pos, reader, BL_DEPTH);
    assert!(digest_eq(&root, blacklist_root));
}

// --- Level-B Viewer Attestation primitives ---

/// Maximum plaintext length for viewer encryption:
/// 32(domain) + 16(value LE) + 32(rho) + 32(recipient) + 32(sender_id) + 128(cm_ins[4]) = 272 bytes.
const NOTE_PLAIN_LEN: usize = 272;

/// Convert a GlDigest to 32 bytes (LE encoding).
fn digest_to_bytes(d: &GlDigest) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..4 {
        let b = d[i].to_le_bytes();
        out[i * 8..(i + 1) * 8].copy_from_slice(&b);
    }
    out
}

/// Convert a u64 to LE bytes.
fn u64_to_le_bytes(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

/// Poseidon2 hash over packed bytes, matching neo-ccs `poseidon2_hash_packed_bytes`.
///
/// Encoding: 8 bytes per Goldilocks element (LE u64), length suffix for framing.
/// Max supported: 504 bytes (63 data elements + 1 length = 64 total).
fn poseidon2_hash_packed_bytes(bytes: &[u8], len: usize) -> GlDigest {
    let n_elems = (len + 7) / 8;
    let mut felts = [0u64; 64];
    let mut i = 0;
    while i < n_elems {
        let off = i * 8;
        let mut buf = [0u8; 8];
        let take = if off + 8 <= len { 8 } else { len - off };
        let mut j = 0;
        while j < take {
            buf[j] = bytes[off + j];
            j += 1;
        }
        felts[i] = u64::from_le_bytes(buf);
        i += 1;
    }
    felts[n_elems] = len as u64;
    poseidon2_hash(&felts[..n_elems + 1])
}

/// FVK commitment: H(b"FVK_COMMIT_V1" || fvk_bytes)
fn view_fvk_commitment(fvk: &GlDigest) -> GlDigest {
    let mut buf = [0u8; 13 + 32]; // tag(13) + fvk(32)
    buf[..13].copy_from_slice(b"FVK_COMMIT_V1");
    buf[13..45].copy_from_slice(&digest_to_bytes(fvk));
    poseidon2_hash_packed_bytes(&buf, 45)
}

/// View KDF: H(b"VIEW_KDF_V1" || fvk_bytes || cm_bytes)
fn view_kdf(fvk: &GlDigest, cm: &GlDigest) -> GlDigest {
    let mut buf = [0u8; 11 + 32 + 32]; // tag(11) + fvk(32) + cm(32)
    buf[..11].copy_from_slice(b"VIEW_KDF_V1");
    buf[11..43].copy_from_slice(&digest_to_bytes(fvk));
    buf[43..75].copy_from_slice(&digest_to_bytes(cm));
    poseidon2_hash_packed_bytes(&buf, 75)
}

/// Stream block: H(b"VIEW_STREAM_V1" || k_bytes || ctr_le_4)
fn view_stream_block(k: &GlDigest, ctr: u32) -> GlDigest {
    let mut buf = [0u8; 14 + 32 + 4]; // tag(14) + k(32) + ctr(4)
    buf[..14].copy_from_slice(b"VIEW_STREAM_V1");
    buf[14..46].copy_from_slice(&digest_to_bytes(k));
    buf[46..50].copy_from_slice(&ctr.to_le_bytes());
    poseidon2_hash_packed_bytes(&buf, 50)
}

/// Ciphertext hash: H(b"CT_HASH_V1" || ct_bytes)
fn view_ct_hash(ct: &[u8; NOTE_PLAIN_LEN]) -> GlDigest {
    let tag = b"CT_HASH_V1";
    let total = tag.len() + NOTE_PLAIN_LEN; // 10 + 272 = 282
    let mut buf = [0u8; 10 + NOTE_PLAIN_LEN];
    buf[..10].copy_from_slice(tag);
    buf[10..10 + NOTE_PLAIN_LEN].copy_from_slice(ct);
    poseidon2_hash_packed_bytes(&buf, total)
}

/// View MAC: H(b"VIEW_MAC_V1" || k_bytes || cm_bytes || ct_hash_bytes)
fn view_mac(k: &GlDigest, cm: &GlDigest, ct_h: &GlDigest) -> GlDigest {
    let mut buf = [0u8; 11 + 32 + 32 + 32]; // tag(11) + k(32) + cm(32) + ct_h(32)
    buf[..11].copy_from_slice(b"VIEW_MAC_V1");
    buf[11..43].copy_from_slice(&digest_to_bytes(k));
    buf[43..75].copy_from_slice(&digest_to_bytes(cm));
    buf[75..107].copy_from_slice(&digest_to_bytes(ct_h));
    poseidon2_hash_packed_bytes(&buf, 107)
}

/// XOR-encrypt plaintext with Poseidon2-based keystream.
fn view_stream_xor_encrypt(k: &GlDigest, pt: &[u8; NOTE_PLAIN_LEN]) -> [u8; NOTE_PLAIN_LEN] {
    let mut ct = [0u8; NOTE_PLAIN_LEN];
    let mut ctr: u32 = 0;
    let mut off: usize = 0;
    while off < NOTE_PLAIN_LEN {
        let ks = view_stream_block(k, ctr);
        let ks_bytes = digest_to_bytes(&ks);
        ctr += 1;
        let take = if off + 32 <= NOTE_PLAIN_LEN { 32 } else { NOTE_PLAIN_LEN - off };
        let mut j = 0;
        while j < take {
            ct[off + j] = pt[off + j] ^ ks_bytes[j];
            j += 1;
        }
        off += take;
    }
    ct
}

/// Encode a note plaintext for viewer encryption (272 bytes).
///
/// Layout: [domain(32) | value_le_16 | rho(32) | recipient(32) | sender_id(32) | cm_ins[4](128)]
fn encode_note_plain(
    domain: &GlDigest,
    value: u64,
    rho: &GlDigest,
    recipient: &GlDigest,
    sender_id: &GlDigest,
    cm_ins: &[GlDigest; MAX_INS],
    n_in: u32,
) -> [u8; NOTE_PLAIN_LEN] {
    let mut pt = [0u8; NOTE_PLAIN_LEN];
    let dom = digest_to_bytes(domain);
    pt[..32].copy_from_slice(&dom);
    // value as 16-byte LE (u64 zero-extended)
    pt[32..40].copy_from_slice(&u64_to_le_bytes(value));
    // bytes 40..48 already zero (padding)
    pt[48..80].copy_from_slice(&digest_to_bytes(rho));
    pt[80..112].copy_from_slice(&digest_to_bytes(recipient));
    pt[112..144].copy_from_slice(&digest_to_bytes(sender_id));
    for i in 0..MAX_INS {
        let off = 144 + i * 32;
        if (i as u32) < n_in {
            pt[off..off + 32].copy_from_slice(&digest_to_bytes(&cm_ins[i]));
        }
        // else: already zero (unused inputs are zero-padded)
    }
    pt
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

    // 4. Nullifiers must be pairwise distinct within the transaction.
    for i in 0..n_in as usize {
        for j in (i + 1)..n_in as usize {
            assert!(!digest_eq(&nullifiers[i], &nullifiers[j]));
        }
    }

    // 5. Read withdraw info
    let withdraw_amount = r.read_u64();
    let withdraw_to = r.read_digest();
    let n_out = r.read_u32();

    // 6. Shape validation rules (matching Ligero circuit):
    //    - Transfer (withdraw_amount == 0): n_out in {1, 2}, withdraw_to must be zero
    //    - Withdraw (withdraw_amount > 0):  n_out in {0, 1}, withdraw_to must be non-zero
    if withdraw_amount == 0 {
        assert!(n_out >= 1);
        assert!(digest_eq(&withdraw_to, &ZERO_DIGEST));
    } else {
        assert!(n_out <= 1);
        assert!(!digest_eq(&withdraw_to, &ZERO_DIGEST));
    }

    // 7. Process outputs: commitments
    let mut out_sum: u64 = GL_ZERO;
    let mut output_values: [u64; MAX_OUTS] = [0; MAX_OUTS];
    let mut output_rhos: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];
    let mut output_cms: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];
    let mut output_rcps: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];

    for j in 0..n_out as usize {
        let value_out = r.read_u64();
        output_values[j] = value_out;
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

    // 8. Balance check: sum_in == withdraw_amount + out_sum
    let rhs = gl_add(withdraw_amount, out_sum);
    assert!(sum_in == rhs);

    // 9. Change output rules
    if withdraw_amount > 0 && n_out == 1 {
        assert!(digest_eq(&output_rcps[0], &sender_id));
    }
    if withdraw_amount == 0 && n_out == 2 {
        assert!(digest_eq(&output_rcps[1], &sender_id));
    }

    // 10. Enforce product: values non-zero, output rhos distinct from input rhos
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

    // 11. Blacklist checks (bucketed non-membership + Merkle membership)
    let blacklist_root = r.read_digest();

    // Sender (current owner) must not be blacklisted.
    assert_not_blacklisted(&sender_id, &blacklist_root, &mut r);

    // For transfers, also check the pay recipient (first output).
    // Withdrawals only produce change-to-self outputs, already enforced above.
    if withdraw_amount == 0 {
        assert_not_blacklisted(&output_rcps[0], &blacklist_root, &mut r);
    }

    // 12. Viewer attestations (Level-B compliance)
    let n_viewers = r.read_u32();

    // Write SpendPublic output (step 13 -- starts output before viewer loop for layout)
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

    // Viewer attestation output: n_viewers, then per-viewer per-output (cm, fvk_commitment, ct_hash, mac)
    w.write_u32(n_viewers);

    for _v in 0..n_viewers as usize {
        // Read viewer private data from witness
        let fvk_commitment_pub = r.read_digest();
        let fvk = r.read_digest();

        // Verify FVK commitment
        let computed_fvk_cm = view_fvk_commitment(&fvk);
        assert!(digest_eq(&computed_fvk_cm, &fvk_commitment_pub));

        for j in 0..n_out as usize {
            // Read public viewer outputs from witness
            let ct_hash_pub = r.read_digest();
            let mac_pub = r.read_digest();

            // Derive keystream key from FVK and output commitment
            let k = view_kdf(&fvk, &output_cms[j]);

            // Encode note plaintext (272 bytes)
            let pt = encode_note_plain(
                &domain,
                output_values[j],
                &output_rhos[j],
                &output_rcps[j],
                &sender_id,
                &input_cms,
                n_in,
            );

            // Encrypt and hash
            let ct = view_stream_xor_encrypt(&k, &pt);
            let ct_h = view_ct_hash(&ct);
            assert!(digest_eq(&ct_h, &ct_hash_pub));

            // Verify MAC
            let mac = view_mac(&k, &output_cms[j], &ct_h);
            assert!(digest_eq(&mac, &mac_pub));

            // Write attestation tuple: (cm, fvk_commitment, ct_hash, mac)
            w.write_digest(&output_cms[j]);
            w.write_digest(&fvk_commitment_pub);
            w.write_digest(&ct_hash_pub);
            w.write_digest(&mac_pub);
        }
    }

    nightstream_sdk::halt();
}
