#![no_std]
#![no_main]

type GlDigest = [u64; 4];

const GL_ZERO: u64 = 0;
const GL_ONE: u64 = 1;
const ZERO_DIGEST: GlDigest = [0, 0, 0, 0];
const GL_MODULUS: u64 = 0xffff_ffff_0000_0001;
const GL_NEG_ORDER: u64 = GL_MODULUS.wrapping_neg();

#[inline]
fn gl_reduce128(x: u128) -> u64 {
    let x_lo = x as u64;
    let x_hi = (x >> 64) as u64;
    let x_hi_hi = x_hi >> 32;
    let x_hi_lo = x_hi & GL_NEG_ORDER;

    let (mut t0, borrow) = x_lo.overflowing_sub(x_hi_hi);
    if borrow {
        t0 = t0.wrapping_sub(GL_NEG_ORDER);
    }

    let t1 = x_hi_lo.wrapping_mul(GL_NEG_ORDER);
    let (t2, carry) = t0.overflowing_add(t1);
    t2.wrapping_add(GL_NEG_ORDER.wrapping_mul(carry as u64))
}

#[inline]
fn gl_canonicalize(x: u64) -> u64 {
    if x >= GL_MODULUS {
        x - GL_MODULUS
    } else {
        x
    }
}

#[inline]
fn gl_eq(a: u64, b: u64) -> bool {
    gl_canonicalize(a) == gl_canonicalize(b)
}

#[inline]
fn gl_add(a: u64, b: u64) -> u64 {
    let (sum, over) = a.overflowing_add(b);
    let (mut sum, over2) = sum.overflowing_add((over as u64) * GL_NEG_ORDER);
    if over2 {
        sum = sum.wrapping_add(GL_NEG_ORDER);
    }
    sum
}

#[inline]
fn gl_sub(a: u64, b: u64) -> u64 {
    let (diff, under) = a.overflowing_sub(b);
    let (mut diff, under2) = diff.overflowing_sub((under as u64) * GL_NEG_ORDER);
    if under2 {
        diff = diff.wrapping_sub(GL_NEG_ORDER);
    }
    diff
}

#[inline]
fn gl_mul(a: u64, b: u64) -> u64 {
    gl_reduce128((a as u128) * (b as u128))
}

#[inline]
fn digest_eq(a: &GlDigest, b: &GlDigest) -> bool {
    a[0] == b[0] && a[1] == b[1] && a[2] == b[2] && a[3] == b[3]
}

#[inline]
fn poseidon2_hash(input: &[u64]) -> GlDigest {
    nightstream_sdk::poseidon2::poseidon2_hash(input)
}

const MAX_INS: usize = 4;
const MAX_OUTS: usize = 2;
#[allow(dead_code)]
const MAX_DEPTH: usize = 63;

const TAG_MT_NODE: u64 = 1;
const TAG_NOTE: u64 = 2;
const TAG_PRF_NF: u64 = 3;
const TAG_PK: u64 = 4;
const TAG_ADDR: u64 = 5;
const TAG_NFKEY: u64 = 6;
const TAG_BL_BUCKET: u64 = 7;

const BL_DEPTH: u32 = 16;
const BL_BUCKET_SIZE: usize = 12;

const INPUT_ADDR: u32 = 0x4104;
const OUTPUT_ADDR: u32 = 0x4100;

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

fn derive_pk_spend(spend_sk: &GlDigest) -> GlDigest {
    let mut input = [0u64; 5];
    input[0] = TAG_PK;
    input[1..5].copy_from_slice(spend_sk);
    poseidon2_hash(&input)
}

fn derive_nf_key(domain: &GlDigest, spend_sk: &GlDigest) -> GlDigest {
    let mut input = [0u64; 9];
    input[0] = TAG_NFKEY;
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(spend_sk);
    poseidon2_hash(&input)
}

fn derive_address(domain: &GlDigest, pk_spend: &GlDigest, pk_ivk: &GlDigest) -> GlDigest {
    let mut input = [0u64; 13];
    input[0] = TAG_ADDR;
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(pk_spend);
    input[9..13].copy_from_slice(pk_ivk);
    poseidon2_hash(&input)
}

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

fn derive_nullifier(domain: &GlDigest, nf_key: &GlDigest, rho: &GlDigest) -> GlDigest {
    let mut input = [0u64; 13];
    input[0] = TAG_PRF_NF;
    input[1..5].copy_from_slice(domain);
    input[5..9].copy_from_slice(nf_key);
    input[9..13].copy_from_slice(rho);
    poseidon2_hash(&input)
}

fn mt_node(level: u64, left: &GlDigest, right: &GlDigest) -> GlDigest {
    let mut input = [0u64; 10];
    input[0] = TAG_MT_NODE;
    input[1] = level;
    input[2..6].copy_from_slice(left);
    input[6..10].copy_from_slice(right);
    poseidon2_hash(&input)
}

fn merkle_root(leaf: &GlDigest, pos: u32, reader: &mut RamReader, depth: u32) -> GlDigest {
    let mut cur = *leaf;
    let mut p = pos;

    let mut lvl = 0u32;
    while lvl < depth {
        let sib = reader.read_digest();
        let bit = (p & 1) as u64;

        let mut left = [0u64; 4];
        let mut right = [0u64; 4];
        let mut i = 0usize;
        while i < 4 {
            let delta = gl_mul(bit, gl_sub(sib[i], cur[i]));
            left[i] = gl_add(cur[i], delta);
            right[i] = gl_sub(sib[i], delta);
            i += 1;
        }

        cur = mt_node(lvl as u64, &left, &right);
        p >>= 1;
        lvl += 1;
    }

    cur
}

fn enforce_prod_digest_diff(acc: u64, a: &GlDigest, b: &GlDigest) -> u64 {
    let mut result = acc;
    let mut i = 0usize;
    while i < 4 {
        let diff = gl_sub(a[i], b[i]);
        result = gl_mul(result, diff);
        i += 1;
    }
    result
}

fn bl_bucket_leaf(entries: &[GlDigest; BL_BUCKET_SIZE]) -> GlDigest {
    let mut input = [0u64; 1 + BL_BUCKET_SIZE * 4];
    input[0] = TAG_BL_BUCKET;
    for (i, entry) in entries.iter().enumerate() {
        let off = 1 + i * 4;
        input[off..off + 4].copy_from_slice(entry);
    }
    poseidon2_hash(&input)
}

fn bl_bucket_pos(id: &GlDigest) -> u32 {
    // Match module-side `blacklist_pos_from_recipient`: take low BL_DEPTH bits
    // from the recipient bytes as little-endian bit order over the 32-byte digest.
    let id_bytes = digest_to_bytes(id);
    let mut pos: u32 = 0;
    let mut i = 0usize;
    while i < BL_DEPTH as usize {
        let byte = id_bytes[31 - (i / 8)];
        let bit = (byte >> (i % 8)) & 1;
        pos |= (bit as u32) << (i as u32);
        i += 1;
    }
    pos
}

fn assert_not_blacklisted(id: &GlDigest, blacklist_root: &GlDigest, reader: &mut RamReader) {
    let mut entries = [ZERO_DIGEST; BL_BUCKET_SIZE];
    for e in entries.iter_mut() {
        *e = reader.read_digest();
    }
    let bucket_inv = reader.read_u64();

    let mut prod: u64 = GL_ONE;
    for entry in &entries {
        prod = enforce_prod_digest_diff(prod, id, entry);
    }
    assert!(gl_eq(gl_mul(prod, bucket_inv), GL_ONE));

    let leaf = bl_bucket_leaf(&entries);
    let pos = bl_bucket_pos(id);
    let root = merkle_root(&leaf, pos, reader, BL_DEPTH);
    assert!(digest_eq(&root, blacklist_root));
}

fn digest_to_bytes(d: &GlDigest) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut i = 0usize;
    while i < 4 {
        let b = d[i].to_le_bytes();
        out[i * 8..(i + 1) * 8].copy_from_slice(&b);
        i += 1;
    }
    out
}

#[nightstream_sdk::provable]
fn note_spend() -> ! {
    let mut r = RamReader::new(INPUT_ADDR);
    let mut w = RamWriter::new(OUTPUT_ADDR);

    let domain = r.read_digest();
    let spend_sk = r.read_digest();
    let pk_ivk_owner = r.read_digest();
    let depth = r.read_u32();
    let anchor = r.read_digest();
    let n_in = r.read_u32();

    assert!(n_in <= MAX_INS as u32);

    let pk_spend_owner = derive_pk_spend(&spend_sk);
    let nf_key = derive_nf_key(&domain, &spend_sk);
    let recipient_owner = derive_address(&domain, &pk_spend_owner, &pk_ivk_owner);
    let sender_id = recipient_owner;

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
        assert!(digest_eq(&root, &anchor));
    }

    let mut nullifiers: [GlDigest; MAX_INS] = [ZERO_DIGEST; MAX_INS];
    for i in 0..n_in as usize {
        let nullifier_pub = r.read_digest();
        let nf = derive_nullifier(&domain, &nf_key, &input_rhos[i]);
        assert!(digest_eq(&nf, &nullifier_pub));
        nullifiers[i] = nullifier_pub;
    }

    for i in 0..n_in as usize {
        for j in (i + 1)..n_in as usize {
            assert!(!digest_eq(&nullifiers[i], &nullifiers[j]));
        }
    }
    let withdraw_amount = r.read_u64();
    let withdraw_to = r.read_digest();
    let n_out = r.read_u32();

    assert!(n_out <= MAX_OUTS as u32);

    if withdraw_amount == 0 {
        assert!(n_out >= 1);
        assert!(digest_eq(&withdraw_to, &ZERO_DIGEST));
    } else {
        assert!(n_out <= 1);
        assert!(!digest_eq(&withdraw_to, &ZERO_DIGEST));
    }

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

    let mut cm_outs_pub: [GlDigest; MAX_OUTS] = [ZERO_DIGEST; MAX_OUTS];
    for j in 0..n_out as usize {
        let cm_pub = r.read_digest();
        assert!(digest_eq(&output_cms[j], &cm_pub));
        cm_outs_pub[j] = cm_pub;
    }

    let rhs = gl_add(withdraw_amount, out_sum);
    assert!(gl_eq(sum_in, rhs));

    if withdraw_amount > 0 && n_out == 1 {
        assert!(digest_eq(&output_rcps[0], &sender_id));
    }
    if withdraw_amount == 0 && n_out == 2 {
        assert!(digest_eq(&output_rcps[1], &sender_id));
    }

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
    assert!(gl_eq(check, GL_ONE));

    let blacklist_root = r.read_digest();
    assert_not_blacklisted(&sender_id, &blacklist_root, &mut r);

    if withdraw_amount == 0 {
        assert_not_blacklisted(&output_rcps[0], &blacklist_root, &mut r);
    }

    let n_viewers = r.read_u32();

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

    w.write_u32(n_viewers);

    for _v in 0..n_viewers as usize {
        let fvk_commitment_pub = r.read_digest();
        let _fvk = r.read_digest();

        for j in 0..n_out as usize {
            let ct_hash_pub = r.read_digest();
            let mac_pub = r.read_digest();

            w.write_digest(&output_cms[j]);
            w.write_digest(&fvk_commitment_pub);
            w.write_digest(&ct_hash_pub);
            w.write_digest(&mac_pub);
        }
    }

    nightstream_sdk::halt();
}
