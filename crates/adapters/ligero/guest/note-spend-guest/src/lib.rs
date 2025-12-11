/*
 * Rust Guest Program: Note Spend Verifier for Midnight Privacy Pool
 *
 * Multi-input join-split: consumes up to 4 notes (same owner), produces up to 2 outputs.
 * Enables note consolidation (4→1) and payments using multiple notes (4→2).
 *
 * Verifies:
 *   1) For each input: Merkle root (anchor) from note commitment + auth path
 *   2) For each input: PRF-based nullifier: Poseidon2("PRF_NF_V1" || domain || nf_key || rho)
 *   3) All inputs owned by same spend_sk (recipient derived, not passed)
 *   4) All nullifiers are distinct within the transaction
 *   5) Output note commitments (0..=2): Poseidon2("NOTE_V1" || domain || value || rho || recipient)
 *   6) Balance: sum(input_values) == withdraw_amount + sum(output_values)
 *   7) Rho uniqueness: output rhos != input rhos, output rhos pairwise distinct
 *      (Prevents creating self-owned notes with reused rho that become unspendable)
 *   8) No zero-value notes (inputs or outputs)
 *   9) Viewer attestations only allowed when n_out > 0
 *
 * AMOUNT-CAP INTERACTION:
 *   - Each input/output value is bounded by u64::MAX
 *   - withdraw_amount is bounded by u64::MAX
 *   - With MAX_INS=4 and MAX_OUTS=2: sum_in can be up to 4*u64::MAX
 *   - But RHS = withdraw + sum_out ≤ 3*u64::MAX (1 withdraw + 2 outputs)
 *   - Therefore: consolidations where sum_in > 3*u64::MAX cannot satisfy balance
 *   - This is acceptable for typical consolidation use cases; use multi-step merges for larger amounts
 *
 * ABI / Arguments (WASI args_get, all ASCII strings):
 *   [1]  domain_hex       — 32-byte hex (PUBLIC)
 *   [2]  spend_sk_hex     — 32-byte hex (PRIVATE) - owner key for ALL inputs
 *   [3]  depth_dec        — u64 decimal (shared depth for all inputs)
 *   [4]  anchor_hex       — 32-byte hex (PUBLIC) - shared Merkle root
 *   [5]  n_in_dec         — u64 decimal in {1..=4} - number of input notes
 *
 *   For each input i in [0..n_in):
 *     value_in_i_dec     — u64 decimal       [PRIVATE]
 *     rho_in_i_hex       — 32-byte hex       [PRIVATE]
 *     pos_in_i_dec       — u64 decimal       [PRIVATE]
 *     siblings_i[k]_hex  — depth items       [PRIVATE]
 *     nullifier_i_hex    — 32-byte hex       [PUBLIC; must equal computed]
 *
 *   Then:
 *     withdraw_amount_dec — u64 decimal      [PUBLIC]
 *     n_out_dec           — u64 decimal in {0,1,2}
 *
 *   For each output j in [0..n_out):
 *     value_out_j_dec    — u64 decimal       [PRIVATE]
 *     rho_out_j_hex      — 32-byte hex       [PRIVATE]
 *     pk_out_j_hex       — 32-byte hex       [PRIVATE] - recipient derived from this
 *     cm_out_j_hex       — 32-byte hex       [PUBLIC; must equal computed]
 *
 *   Optional viewer attestations (unchanged from before):
 *     n_viewers_dec      — u32 in {0..=8}
 *     For each viewer:
 *       fvk_commit_hex   — 32-byte hex [PUBLIC]
 *       fvk_hex          — 32-byte hex [PRIVATE]
 *       For each output j:
 *         ct_hash_j_hex  — 32-byte hex [PUBLIC]
 *         mac_j_hex      — 32-byte hex [PUBLIC]
 *
 * Use cases:
 *   - Consolidation (merge): n_in=4, n_out=1, output to self = sum(inputs)
 *   - Payment with change:   n_in=1..4, n_out=2 (recipient + change to self)
 *   - Exact payment:         n_in=1..4, n_out=1 (full value to recipient)
 *   - Withdraw all:          n_in=1..4, n_out=0, withdraw=sum(inputs)
 *
 * Hashing uses qp_poseidon_core::Poseidon2Core exactly like on-chain.
 *
 * Viewer plaintexts (Level B) include an attested sender_id:
 *   [ domain | value | rho | recipient | sender_id ]
 * 
 * Copyright (C) 2023-2025 Sovereign Labs
 * Licensed under the Apache License, Version 2.0
 */

use core::mem::MaybeUninit;

#[link(wasm_import_module = "env")]
extern "C" {
    fn assert_one(x: i32);
}

#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn args_sizes_get(argc: *mut u32, argv_buf_size: *mut u32) -> u32;
    fn args_get(argv_ptrs: *mut *mut u8, argv_buf: *mut u8) -> u32;
    fn proc_exit(code: u32) -> !;
}

use qp_poseidon_core::Poseidon2Core;

type Hash32 = [u8; 32];

#[inline(always)]
fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut acc = 0u8;
    for i in 0..a.len() { acc |= a[i] ^ b[i]; }
    acc == 0
}

#[inline(always)]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[inline(always)]
unsafe fn read_cstr(mut p: *const u8, out: &mut [u8]) -> usize {
    let mut i = 0usize;
    while i < out.len() {
        let c = core::ptr::read_unaligned(p);
        if c == 0 { break; }
        out[i] = c;
        i += 1;
        p = p.add(1);
    }
    i
}

#[inline(always)]
fn parse_hex32(s: &[u8]) -> Option<Hash32> {
    let mut i = 0usize;
    while i < s.len() && s[i] == b' ' { i += 1; }
    let s = &s[i..];
    let rest = if s.len() >= 2 && s[0] == b'0' && (s[1] == b'x' || s[1] == b'X') { &s[2..] } else { s };
    if rest.len() != 64 { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(rest[2*i])?;
        let lo = hex_nibble(rest[2*i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

/// Parse decimal u64. Only allows leading/trailing whitespace, not embedded spaces.
#[inline(always)]
fn parse_u64_dec(s: &[u8]) -> Option<u64> {
    if s.is_empty() { return None; }
    // Skip leading whitespace
    let mut start = 0usize;
    while start < s.len() && (s[start] == b' ' || s[start] == 0) {
        if s[start] == 0 { return if start == 0 { None } else { Some(0) }; }
        start += 1;
    }
    if start == s.len() { return None; } // all whitespace
    // Find end of digits
    let mut end = start;
    while end < s.len() && s[end] != 0 && s[end] != b' ' {
        if !(b'0'..=b'9').contains(&s[end]) { return None; }
        end += 1;
    }
    if end == start { return None; } // no digits
    // Check trailing is only whitespace or null
    let mut trail = end;
    while trail < s.len() && s[trail] != 0 {
        if s[trail] != b' ' { return None; } // non-whitespace after digits
        trail += 1;
    }
    // Parse digits
    let mut v: u64 = 0;
    for i in start..end {
        v = v.checked_mul(10)?.checked_add((s[i] - b'0') as u64)?;
    }
    Some(v)
}

/// Parse decimal u128. Only allows leading/trailing whitespace, not embedded spaces.
/// Note: Currently unused (values parsed as u64), but kept for potential future use.
#[allow(dead_code)]
#[inline(always)]
fn parse_u128_dec(s: &[u8]) -> Option<u128> {
    if s.is_empty() { return None; }
    // Skip leading whitespace
    let mut start = 0usize;
    while start < s.len() && (s[start] == b' ' || s[start] == 0) {
        if s[start] == 0 { return if start == 0 { None } else { Some(0) }; }
        start += 1;
    }
    if start == s.len() { return None; } // all whitespace
    // Find end of digits
    let mut end = start;
    while end < s.len() && s[end] != 0 && s[end] != b' ' {
        if !(b'0'..=b'9').contains(&s[end]) { return None; }
        end += 1;
    }
    if end == start { return None; } // no digits
    // Check trailing is only whitespace or null
    let mut trail = end;
    while trail < s.len() && s[trail] != 0 {
        if s[trail] != b' ' { return None; } // non-whitespace after digits
        trail += 1;
    }
    // Parse digits
    let mut v: u128 = 0;
    for i in start..end {
        v = v.checked_mul(10)?.checked_add((s[i] - b'0') as u128)?;
    }
    Some(v)
}

/// Maximum buffer size for hash inputs (stack-allocated, no heap).
/// Covers: NOTE_V1 (119 bytes), CT_HASH_V1 (~160 bytes), all other tags smaller.
const HASH_BUF_LEN: usize = 256;

fn poseidon2_hash_domain(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    // Compute total length
    let mut len = tag.len();
    for p in parts {
        len += p.len();
    }

    // Defensive bound: should never trigger with current tags/inputs.
    if len > HASH_BUF_LEN {
        unsafe { proc_exit(71); }
    }

    // Small stack buffer, no heap allocation.
    let mut buf = [0u8; HASH_BUF_LEN];
    let mut offset = 0;

    // Copy tag
    buf[offset..offset + tag.len()].copy_from_slice(tag);
    offset += tag.len();

    // Copy parts
    for p in parts {
        let pl = p.len();
        buf[offset..offset + pl].copy_from_slice(p);
        offset += pl;
    }

    // Fresh hasher each time (same semantics as before).
    let hasher = Poseidon2Core::new();
    hasher.hash_padded(&buf[..len])
}

fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
    let lvl = [level];
    poseidon2_hash_domain(b"MT_NODE_V1", &[&lvl, left, right])
}

fn note_commitment(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32) -> Hash32 {
    let v = value.to_le_bytes();
    poseidon2_hash_domain(b"NOTE_V1", &[domain, &v, rho, recipient])
}

fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"PRF_NF_V1", &[domain, nf_key, rho])
}

// === Spend authorization / identity derivations (Poseidon-only) ===

/// pk = H("PK_V1" || spend_sk)
#[inline(always)]
fn pk_from_sk(spend_sk: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"PK_V1", &[spend_sk])
}

/// recipient_addr = H("ADDR_V1" || domain || pk)
#[inline(always)]
fn recipient_from_pk(domain: &Hash32, pk: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"ADDR_V1", &[domain, pk])
}

/// recipient_addr from spend_sk (convenience: sk -> pk -> recipient)
#[inline(always)]
fn recipient_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let pk = pk_from_sk(spend_sk);
    recipient_from_pk(domain, &pk)
}

/// nf_key = H("NFKEY_V1" || domain || spend_sk)
#[inline(always)]
fn nf_key_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"NFKEY_V1", &[domain, spend_sk])
}

fn root_from_path(leaf: &Hash32, pos: u64, siblings: &[Hash32], depth: u32) -> Hash32 {
    let mut cur = *leaf;
    let mut idx = pos;
    for (lvl, sib) in (0..depth).zip(siblings.iter()) {
        cur = if (idx & 1) == 0 { mt_combine(lvl as u8, &cur, sib) }
              else               { mt_combine(lvl as u8, sib, &cur) };
        idx >>= 1;
    }
    cur
}

// === Level B: Viewer Attestation Functions ===

/// Compute FVK commitment: H("FVK_COMMIT_V1" || fvk)
#[inline(always)]
fn fvk_commit(fvk: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"FVK_COMMIT_V1", &[fvk])
}

/// Derive per-note viewing key: H("VIEW_KDF_V1" || fvk || cm)
#[inline(always)]
fn view_kdf(fvk: &Hash32, cm: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"VIEW_KDF_V1", &[fvk, cm])
}

/// Produce the i-th 32-byte stream block for key k using Poseidon2.
/// (i is a u32 counter, LE-encoded)
#[inline(always)]
fn stream_block(k: &Hash32, ctr: u32) -> Hash32 {
    let c = ctr.to_le_bytes();
    poseidon2_hash_domain(b"VIEW_STREAM_V1", &[k, &c])
}

/// SNARK-friendly deterministic encryption: XOR plaintext with Poseidon-based keystream.
/// `pt` and `ct_out` must have equal length.
fn stream_xor_encrypt(k: &Hash32, pt: &[u8], ct_out: &mut [u8]) {
    debug_assert_eq!(pt.len(), ct_out.len());
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < pt.len() {
        let ks = stream_block(k, ctr);
        ctr = ctr.wrapping_add(1);

        let take = core::cmp::min(32, pt.len() - off);
        for i in 0..take {
            ct_out[off + i] = pt[off + i] ^ ks[i];
        }
        off += take;
    }
}

/// Compute ciphertext hash: H("CT_HASH_V1" || ct)
#[inline(always)]
fn ct_hash(ct: &[u8]) -> Hash32 {
    poseidon2_hash_domain(b"CT_HASH_V1", &[ct])
}

/// Compute viewing MAC: H("VIEW_MAC_V1" || k || cm || ct_hash)
#[inline(always)]
fn view_mac(k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"VIEW_MAC_V1", &[k, cm, ct_h])
}

/// Deterministic serialization of a Note plaintext used for encryption:
/// [ domain(32) | value_le_16 | rho(32) | recipient(32) | sender_id(32) ] => 144 bytes
fn encode_note_plain(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32, sender_id: &Hash32, out: &mut [u8; 144]) {
    out[0..32].copy_from_slice(domain);
    out[32..48].copy_from_slice(&value.to_le_bytes());
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
}

const MAX_ARGS: usize = 512;
const MAX_BUF: usize = 128 * 1024;
/// Maximum Merkle tree depth supported by the circuit.
/// Must be ≤ 63 to ensure the bound check `pos >= (1u64 << depth)` is safe
/// (shifting by 64 would overflow a u64).
const MAX_DEPTH: usize = 63;
/// Maximum number of input notes (same-owner consolidation).
const MAX_INS: usize = 4;
const MAX_OUTS: usize = 2;
const MAX_VIEWERS: usize = 8;
const NOTE_PLAIN_LEN: usize = 144; // 32 + 16 + 32 + 32 + 32 (domain + value + rho + recipient + sender_id)

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    let mut argc: u32 = 0;
    let mut buf_len: u32 = 0;
    if args_sizes_get(&mut argc, &mut buf_len) != 0 { proc_exit(71); }
    if (argc as usize) > MAX_ARGS || (buf_len as usize) > MAX_BUF { proc_exit(71); }

    let mut ptrs: [*mut u8; MAX_ARGS] = [core::ptr::null_mut(); MAX_ARGS];
    
    // Uninitialized buffer - WASI fills the used prefix. Avoids 128KB zero-init.
    let mut buf = MaybeUninit::<[u8; MAX_BUF]>::uninit();
    if args_get(ptrs.as_mut_ptr(), buf.as_mut_ptr() as *mut u8) != 0 { proc_exit(71); }
    // ptrs[i] now point into initialized memory; we read via ptrs, never buf directly.

    let mut tmp = [0u8; 256];
    let mut arg_idx: usize = 1;

    // ========== EARLY ARGC CHECK: HEADER ==========
    // Need at least argv[0] + 5 header args (domain, spend_sk, depth, anchor, n_in)
    if argc < 6 { proc_exit(71); }

    // ========== HEADER ARGUMENTS ==========

    // [1] domain (PUBLIC)
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let domain = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    arg_idx += 1;

    // [2] spend_sk (PRIVATE) - owner key for ALL inputs
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let spend_sk = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    arg_idx += 1;

    // [3] depth (shared for all inputs)
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let depth_u64 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    if depth_u64 > MAX_DEPTH as u64 { proc_exit(71); }
    let depth_u32 = depth_u64 as u32;
    let depth = depth_u32 as usize;
    arg_idx += 1;

    // [4] anchor (PUBLIC) - shared Merkle root for all inputs
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let anchor_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    arg_idx += 1;

    // [5] n_in - number of input notes (1..=4)
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let n_in_u64 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    if n_in_u64 == 0 || n_in_u64 > MAX_INS as u64 { proc_exit(71); }
    let n_in_u32 = n_in_u64 as u32;
    let n_in = n_in_u32 as usize;
    arg_idx += 1;

    // ========== EARLY ARGC CHECK: INPUTS + WITHDRAW + N_OUT ==========
    // Per input: 4 + depth args (value, rho, pos, siblings[depth], nullifier)
    // After inputs: 2 args (withdraw, n_out)
    let per_in = depth_u32.checked_add(4).unwrap_or_else(|| proc_exit(71));
    let expected_min_to_n_out = 6u32
        .checked_add(n_in_u32.checked_mul(per_in).unwrap_or_else(|| proc_exit(71))).unwrap_or_else(|| proc_exit(71))
        .checked_add(2u32).unwrap_or_else(|| proc_exit(71));
    if argc < expected_min_to_n_out { proc_exit(71); }

    // Derive recipient + nf_key once for all inputs (same-owner multi-input)
    let recipient_owner = recipient_from_sk(&domain, &spend_sk);
    let sender_id = recipient_owner; // For viewer attestations
    let nf_key = nf_key_from_sk(&domain, &spend_sk);

    // ========== INPUT NOTES ==========
    // Each input: value, rho, pos, siblings[depth], nullifier

    let mut sum_in: u128 = 0;
    let mut nullifiers: [Hash32; MAX_INS] = [[0u8; 32]; MAX_INS];
    let mut input_rhos: [Hash32; MAX_INS] = [[0u8; 32]; MAX_INS]; // Store for rho reuse prevention
    let mut siblings: [Hash32; MAX_DEPTH] = [[0u8; 32]; MAX_DEPTH];

    for i in 0..n_in {
        // value_in_i (PRIVATE) - parse as u64 for efficiency (bank bound implicit)
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let v_i_u64 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        if v_i_u64 == 0 { proc_exit(71); } // reject zero-value input notes
        let v_i = v_i_u64 as u128; // widen to u128 for sum
        sum_in = sum_in.checked_add(v_i).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // rho_in_i (PRIVATE)
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let rho_i = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        input_rhos[i] = rho_i; // Store for rho reuse prevention
        arg_idx += 1;

        // pos_in_i (PRIVATE)
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let pos_i = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        if pos_i >= (1u64 << depth_u32) { proc_exit(71); }
        arg_idx += 1;

        // siblings_i[0..depth) (PRIVATE)
        for k in 0..depth {
            let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
            if n == tmp.len() { proc_exit(71); } // reject truncation
            siblings[k] = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
            arg_idx += 1;
        }

        // nullifier_i (PUBLIC)
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let nf_arg_i = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // Verify Merkle membership: cm_i -> anchor (shared for all inputs)
        let cm_i = note_commitment(&domain, v_i, &rho_i, &recipient_owner);
        let anchor_i = root_from_path(&cm_i, pos_i, &siblings[..depth], depth_u32);
        assert_one(eq_bytes(&anchor_i, &anchor_arg) as i32);

        // Verify nullifier: derived nf_key + per-input rho
        let nf_i = nullifier(&domain, &nf_key, &rho_i);
        assert_one(eq_bytes(&nf_i, &nf_arg_i) as i32);

        nullifiers[i] = nf_i;
    }

    // Prevent double-counting: all nullifiers must be distinct within this proof
    for i in 0..n_in {
        for j in (i + 1)..n_in {
            assert_one((!eq_bytes(&nullifiers[i], &nullifiers[j])) as i32);
        }
    }

    // ========== WITHDRAW + OUTPUTS ==========

    // withdraw_amount (PUBLIC) - parse as u64 for efficiency (bank bound implicit)
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let withdraw_u64 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    let withdraw_amount = withdraw_u64 as u128; // widen to u128 for balance equation
    arg_idx += 1;

    // n_out in {0,1,2}
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let n_out_u64 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    if n_out_u64 > MAX_OUTS as u64 { proc_exit(71); }
    let n_out_u32 = n_out_u64 as u32;
    let n_out = n_out_u32 as usize;
    arg_idx += 1;

    // ========== EARLY ARGC CHECK: OUTPUTS ==========
    // Per output: 4 args (value, rho, pk, cm)
    let expected_base = expected_min_to_n_out
        .checked_add(4u32.checked_mul(n_out_u32).unwrap_or_else(|| proc_exit(71))).unwrap_or_else(|| proc_exit(71));
    if argc < expected_base { proc_exit(71); }

    // Store output data for viewer encryption
    struct OutPlain {
        v: u128,
        rho: Hash32,
        rcp: Hash32,
        cm: Hash32,
    }
    let mut outs: [OutPlain; MAX_OUTS] = [
        OutPlain { v: 0, rho: [0; 32], rcp: [0; 32], cm: [0; 32] },
        OutPlain { v: 0, rho: [0; 32], rcp: [0; 32], cm: [0; 32] },
    ];

    let mut out_sum: u128 = 0;
    for j in 0..n_out {
        // value_out_j (PRIVATE) - parse as u64 for efficiency (bank bound implicit)
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let vj_u64 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        if vj_u64 == 0 { proc_exit(71); } // reject zero-value output notes
        let vj = vj_u64 as u128; // widen to u128 for sum
        out_sum = out_sum.checked_add(vj).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // rho_out_j (PRIVATE)
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let rho_j = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // pk_out_j (PRIVATE) - recipient is derived from this
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let pk_out_j = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // Derive recipient from pk_out_j
        let rcp_j = recipient_from_pk(&domain, &pk_out_j);

        // cm_out_j (PUBLIC) - must equal computed
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let cm_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        let cm_cmp = note_commitment(&domain, vj, &rho_j, &rcp_j);
        assert_one(eq_bytes(&cm_cmp, &cm_arg) as i32);

        outs[j] = OutPlain { v: vj, rho: rho_j, rcp: rcp_j, cm: cm_arg };
    }

    // ========== RHO REUSE PREVENTION ==========
    // Prevent creating self-owned notes with same rho as spent inputs (would be unspendable)
    // Check: output rho != any input rho
    for j in 0..n_out {
        for i in 0..n_in {
            assert_one((!eq_bytes(&outs[j].rho, &input_rhos[i])) as i32);
        }
    }
    // Check: output rhos are pairwise distinct
    for a in 0..n_out {
        for b in (a + 1)..n_out {
            assert_one((!eq_bytes(&outs[a].rho, &outs[b].rho)) as i32);
        }
    }

    // ========== BALANCE CHECK ==========
    // sum(inputs) == withdraw + sum(outputs)
    let rhs = withdraw_amount.checked_add(out_sum).unwrap_or_else(|| proc_exit(71));
    assert_one((sum_in == rhs) as i32);

    // ========== VIEWER ATTESTATIONS (Level B) ==========
    // Optional: verify ct_hash + mac for each (output, viewer)

    // If we have exactly the base args, no viewer attestations
    if argc == expected_base {
        proc_exit(0);
    }

    // Disallow viewer attestations when n_out == 0 (pointless, just adds calldata)
    if n_out == 0 { proc_exit(71); }

    // Otherwise argc > expected_base, so we must have viewer attestations
    let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
    if n == tmp.len() { proc_exit(71); } // reject truncation
    let n_viewers: usize = {
        let v = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71)) as usize;
        if v > MAX_VIEWERS { proc_exit(71); }
        v
    };
    arg_idx += 1;

    // Expected argc with viewer attestations:
    //   expected_base + 1 (n_viewers)
    //   + n_viewers * ( 1 public fvk_commit + 1 private fvk + 2*n_out public digests )
    let extra_per_viewer = 1 + 1 + 2 * n_out;
    let expected_argc_b = expected_base
        .checked_add(1u32).unwrap_or_else(|| proc_exit(71))
        .checked_add((n_viewers as u32).checked_mul(extra_per_viewer as u32).unwrap_or_else(|| proc_exit(71))).unwrap_or_else(|| proc_exit(71));
    if argc != expected_argc_b { proc_exit(71); }

    // Work buffer for note plaintext and ciphertext
    let mut pt_buf = [0u8; NOTE_PLAIN_LEN];
    let mut ct_buf = [0u8; NOTE_PLAIN_LEN];

    for _v in 0..n_viewers {
        // 1) Public fvk_commitment
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let fvk_commit_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // 2) Private fvk
        let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
        if n == tmp.len() { proc_exit(71); } // reject truncation
        let fvk = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        arg_idx += 1;

        // Check binding H(fvk) == fvk_commitment (public)
        let fvk_c = fvk_commit(&fvk);
        assert_one(eq_bytes(&fvk_c, &fvk_commit_arg) as i32);

        // 3) For each output, compute ct_hash + mac and compare to public args
        for j in 0..n_out {
            let outp = &outs[j];

            // Serialize plaintext with sender_id (derived from spend_sk)
            encode_note_plain(&domain, outp.v, &outp.rho, &outp.rcp, &sender_id, &mut pt_buf);

            // Key from (fvk, cm_j)
            let k = view_kdf(&fvk, &outp.cm);

            // Encrypt deterministically
            stream_xor_encrypt(&k, &pt_buf, &mut ct_buf);

            // Compute digests
            let ct_h = ct_hash(&ct_buf);
            let macv = view_mac(&k, &outp.cm, &ct_h);

            // Parse and assert ct_hash (PUBLIC)
            let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
            if n == tmp.len() { proc_exit(71); } // reject truncation
            let ct_hash_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
            arg_idx += 1;
            assert_one(eq_bytes(&ct_h, &ct_hash_arg) as i32);

            // Parse and assert mac (PUBLIC)
            let n = read_cstr(ptrs[arg_idx] as *const u8, &mut tmp);
            if n == tmp.len() { proc_exit(71); } // reject truncation
            let mac_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
            arg_idx += 1;
            assert_one(eq_bytes(&macv, &mac_arg) as i32);
        }
    }

    proc_exit(0)
}
