/*
 * Rust Guest Program: Note Spend Verifier for Midnight Privacy Pool (outputs + balance)
 *
 * Verifies a single-input spend with up to two shielded outputs:
 *   1) Merkle root (anchor) from note commitment + auth path
 *   2) PRF-based nullifier: Poseidon2("PRF_NF_V1" || domain || nf_key || rho)
 *   3) Output note commitments (0..=2): Poseidon2("NOTE_V1" || domain || value || rho || recipient)
 *   4) Balance: input_value == withdraw_amount + sum(output_values)
 *
 * ABI / Arguments (WASI args_get, all ASCII strings):
 *   [1]  domain_hex                 — 32-byte hex
 *   [2]  value_dec                  — input note value (u128; asserted ≤ u64::MAX)
 *   [3]  rho_hex                    — 32-byte hex
 *   [4]  recipient_hex              — 32-byte hex  [PRIVATE]
 *   [5]  nf_key_hex                 — 32-byte hex  [PRIVATE]
 *   [6]  pos_dec                    — u64          [PRIVATE]
 *   [7]  depth_dec                  — u32
 *   [8..8+depth) siblings_hex[i]    — 32-byte hex each   [PRIVATE]
 *   [8+depth]   anchor_hex          — expected Merkle root (32-byte hex)
 *   [9+depth]   nullifier_hex       — expected nullifier  (32-byte hex)
 *   [10+depth]  withdraw_amount_dec — u128 (asserted ≤ u64::MAX)
 *   [11+depth]  n_out_dec           — u32 in {0,1,2}
 *   For each j in [0..n_out):
 *     [12+depth + 4*j + 0] value_out_j_dec     — u128               [PRIVATE]
 *     [12+depth + 4*j + 1] rho_out_j_hex       — 32-byte hex        [PRIVATE]
 *     [12+depth + 4*j + 2] recipient_out_j_hex — 32-byte hex        [PRIVATE]
 *     [12+depth + 4*j + 3] cm_out_j_hex        — 32-byte hex (PUBLIC; must equal computed)
 *
 * Expected argc = 12 + depth + 4*n_out (argc includes argv[0]).
 *
 * Hashing uses qp_poseidon_core::Poseidon2Core exactly like on-chain.
 * 
 * Copyright (C) 2023-2025 Sovereign Labs
 * Licensed under the Apache License, Version 2.0
 */

use std::vec::Vec;

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

#[inline(always)]
fn parse_u64_dec(s: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    if s.is_empty() { return None; }
    for &c in s {
        if c == 0 { break; }
        if c == b' ' { continue; }
        if !(b'0'..=b'9').contains(&c) { return None; }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

#[inline(always)]
fn parse_u128_dec(s: &[u8]) -> Option<u128> {
    let mut v: u128 = 0;
    if s.is_empty() { return None; }
    for &c in s {
        if c == 0 { break; }
        if c == b' ' { continue; }
        if !(b'0'..=b'9').contains(&c) { return None; }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u128)?;
    }
    Some(v)
}

fn get_hasher() -> Poseidon2Core {
    Poseidon2Core::new()
}

fn poseidon2_hash_domain(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    let hasher = get_hasher();
    let mut len = tag.len();
    for p in parts { len += p.len(); }
    let mut tmp = Vec::with_capacity(len);
    tmp.extend_from_slice(tag);
    for p in parts { tmp.extend_from_slice(p); }
    hasher.hash_padded(&tmp)
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

const MAX_ARGS: usize = 256;
const MAX_BUF: usize = 64 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_OUTS: usize = 2;

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    let mut argc: u32 = 0;
    let mut buf_len: u32 = 0;
    if args_sizes_get(&mut argc, &mut buf_len) != 0 { proc_exit(71); }
    if (argc as usize) > MAX_ARGS || (buf_len as usize) > MAX_BUF { proc_exit(71); }

    let mut ptrs: [*mut u8; MAX_ARGS] = [core::ptr::null_mut(); MAX_ARGS];
    let mut buf: [u8; MAX_BUF] = [0; MAX_BUF];
    if args_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) != 0 { proc_exit(71); }

    let mut tmp = [0u8; 256];

    // 1) domain
    let n = read_cstr(ptrs[1] as *const u8, &mut tmp);
    let domain = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    // 2) input value (u128) — enforce bank bound (≤ u64::MAX)
    let n = read_cstr(ptrs[2] as *const u8, &mut tmp);
    let value = parse_u128_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    if value > u64::MAX as u128 { proc_exit(71); }

    // 3) rho
    let n = read_cstr(ptrs[3] as *const u8, &mut tmp);
    let rho = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    // 4) recipient [PRIVATE]
    let n = read_cstr(ptrs[4] as *const u8, &mut tmp);
    let recipient = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    // 5) nf_key [PRIVATE]
    let n = read_cstr(ptrs[5] as *const u8, &mut tmp);
    let nf_key = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    // 6) pos [PRIVATE]
    let n = read_cstr(ptrs[6] as *const u8, &mut tmp);
    let pos = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    // 7) depth
    let n = read_cstr(ptrs[7] as *const u8, &mut tmp);
    let depth_u32 = parse_u64_dec(&tmp[..n]).map(|x| x as u32).unwrap_or_else(|| proc_exit(71));
    if depth_u32 as usize > MAX_DEPTH { proc_exit(71); }
    let depth = depth_u32 as usize;

    // position range for this depth
    if pos >= (1u64 << depth_u32) { proc_exit(71); }

    // siblings [PRIVATE]
    let mut siblings: [Hash32; MAX_DEPTH] = [[0u8; 32]; MAX_DEPTH];
    for i in 0..depth {
        let n = read_cstr(ptrs[8 + i] as *const u8, &mut tmp);
        siblings[i] = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    }

    // anchor and nullifier args
    let n = read_cstr(ptrs[8 + depth] as *const u8, &mut tmp);
    let anchor_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    let n = read_cstr(ptrs[9 + depth] as *const u8, &mut tmp);
    let nullifier_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

    // withdraw amount
    let n = read_cstr(ptrs[10 + depth] as *const u8, &mut tmp);
    let withdraw_amount = parse_u128_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
    if withdraw_amount > u64::MAX as u128 { proc_exit(71); }

    // n_out in {0,1,2}
    let n = read_cstr(ptrs[11 + depth] as *const u8, &mut tmp);
    let n_out_u32 = parse_u64_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71)) as u32;
    if n_out_u32 as usize > MAX_OUTS { proc_exit(71); }
    let n_out = n_out_u32 as usize;

    // argc check once we know n_out (argc includes argv[0])
    let expected_argc = 12u32 + depth_u32 + 4u32 * n_out_u32;
    if argc != expected_argc { proc_exit(71); }

    // Parse & verify outputs
    let mut out_sum: u128 = 0;
    for j in 0..n_out {
        let base = 12 + depth + 4 * j;

        // value_out_j [PRIVATE]
        let n = read_cstr(ptrs[base + 0] as *const u8, &mut tmp);
        let vj = parse_u128_dec(&tmp[..n]).unwrap_or_else(|| proc_exit(71));
        if vj > u64::MAX as u128 { proc_exit(71); } // bank bound
        out_sum = out_sum.checked_add(vj).unwrap_or_else(|| proc_exit(71));

        // rho_out_j [PRIVATE]
        let n = read_cstr(ptrs[base + 1] as *const u8, &mut tmp);
        let rho_j = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

        // recipient_out_j [PRIVATE]
        let n = read_cstr(ptrs[base + 2] as *const u8, &mut tmp);
        let rcp_j = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

        // cm_out_j (PUBLIC) — must equal computed commitment
        let n = read_cstr(ptrs[base + 3] as *const u8, &mut tmp);
        let cm_arg = parse_hex32(&tmp[..n]).unwrap_or_else(|| proc_exit(71));

        let cm_cmp = note_commitment(&domain, vj, &rho_j, &rcp_j);
        assert_one(eq_bytes(&cm_cmp, &cm_arg) as i32);
    }

    // Compute input note commitment and anchor
    let cm_in = note_commitment(&domain, value, &rho, &recipient);
    let anchor_computed = root_from_path(&cm_in, pos, &siblings[..depth], depth_u32);
    assert_one(eq_bytes(&anchor_computed, &anchor_arg) as i32);

    // Compute PRF nullifier and check
    let nf = nullifier(&domain, &nf_key, &rho);
    assert_one(eq_bytes(&nf, &nullifier_arg) as i32);

    // Balance: input value must equal withdraw + sum(outputs)
    let rhs = withdraw_amount.checked_add(out_sum).unwrap_or_else(|| proc_exit(71));
    assert_one((value == rhs) as i32);

    proc_exit(0)
}
