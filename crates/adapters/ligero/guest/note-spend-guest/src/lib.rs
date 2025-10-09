/*
 * Rust Guest Program: Note Spend Verifier for Midnight Privacy Pool
 * 
 * This WASI-command program verifies a single-input note spend for a shielded pool.
 * 
 * It verifies:
 *   1. Merkle root recomputation from a note commitment and authentication path
 *   2. PRF-based nullifier computation (Poseidon2 over domain||nf_key||rho)
 *   3. Public value binding:
 *      - computed anchor_root == argv anchor_root
 *      - computed nullifier == argv nullifier
 *      - withdraw_amount ≤ note.value
 * 
 * Uses Poseidon2 over the Goldilocks field with WIDTH=12, RATE=4, matching the
 * exact padding and domain-separation scheme of qp_poseidon_core used on-chain.
 * This ensures that hashes produced here match those produced in the module.
 * 
 * ABI / Arguments (WASI args_get, all ASCII strings):
 *   [1] domain_hex           — 32-byte hex (64 chars) domain tag
 *   [2] value_dec            — note value as decimal u128
 *   [3] rho_hex              — 32-byte hex nonce
 *   [4] recipient_hex        — 32-byte hex recipient binding
 *   [5] nf_key_hex           — 32-byte hex nullifier secret [PRIVATE in prover config]
 *   [6] pos_dec              — note leaf index as decimal u64 [PRIVATE in prover config]
 *   [7] depth_dec            — Merkle depth as decimal u32
 *   [8..8+depth) siblings_hex[i] — each is 32-byte hex [PRIVATE in prover config]
 *   [8+depth] anchor_hex     — expected Merkle root as 32-byte hex
 *   [9+depth] nullifier_hex  — expected nullifier as 32-byte hex
 *   [10+depth] withdraw_amount_dec — decimal u128 (must be ≤ value)
 * 
 * Note: Uses std for qp-poseidon-core compatibility. WASM target size is still reasonable.
 * 
 * Copyright (C) 2023-2025 Sovereign Labs
 * Licensed under the Apache License, Version 2.0
 */

use std::vec::Vec;

// ---- WASI + host imports ----

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

// ---- Poseidon2 hashing via qp-poseidon-core ----

use qp_poseidon_core::Poseidon2Core;

// ---- Utilities ----

type Hash32 = [u8; 32];

#[inline(always)]
fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for i in 0..a.len() {
        acc |= a[i] ^ b[i];
    }
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
    // copy until NUL or out full; return length
    let mut i = 0usize;
    while i < out.len() {
        let c = core::ptr::read_unaligned(p);
        if c == 0 {
            break;
        }
        out[i] = c;
        i += 1;
        p = p.add(1);
    }
    i
}

#[inline(always)]
fn parse_hex32(s: &[u8]) -> Option<Hash32> {
    // Trim leading spaces
    let mut i = 0usize;
    while i < s.len() && s[i] == b' ' {
        i += 1;
    }
    let s = &s[i..];
    
    // Allow optional "0x" prefix
    let (_start, rest) = if s.len() >= 2 && s[0] == b'0' && (s[1] == b'x' || s[1] == b'X') {
        (2usize, &s[2..])
    } else {
        (0usize, s)
    };
    
    // STRICT: require EXACTLY 64 hex chars, no more, no less
    if rest.len() != 64 {
        return None;
    }
    
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(rest[2 * i])?;
        let lo = hex_nibble(rest[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

#[inline(always)]
fn parse_u64_dec(s: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    if s.is_empty() {
        return None;
    }
    for &c in s {
        if c == 0 {
            break;
        }
        if c == b' ' {
            continue;
        }
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        let d = (c - b'0') as u64;
        v = v.checked_mul(10)?.checked_add(d)?;
    }
    Some(v)
}

#[inline(always)]
fn parse_u128_dec(s: &[u8]) -> Option<u128> {
    let mut v: u128 = 0;
    if s.is_empty() {
        return None;
    }
    for &c in s {
        if c == 0 {
            break;
        }
        if c == b' ' {
            continue;
        }
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        let d = (c - b'0') as u128;
        v = v.checked_mul(10)?.checked_add(d)?;
    }
    Some(v)
}

// ---- Poseidon2 hashing using qp_poseidon_core (matches on-chain exactly) ----

// Global hasher instance
fn get_hasher() -> Poseidon2Core {
    Poseidon2Core::new()
}

// Domain-separated hash helper: H(tag || parts...)
fn poseidon2_hash_domain(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
    let hasher = get_hasher();
    
    // Concatenate tag and all parts
    let mut buf_len = tag.len();
    for part in parts {
        buf_len += part.len();
    }
    let mut tmp = Vec::with_capacity(buf_len);
    tmp.extend_from_slice(tag);
    for part in parts {
        tmp.extend_from_slice(part);
    }
    
    hasher.hash_padded(&tmp)
}

// ---- Pool-specific hashing (must match on-chain) ----

fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
    let lvl = [level];
    poseidon2_hash_domain(b"MT_NODE_V1", &[&lvl, left, right])
}

fn note_commitment(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32) -> Hash32 {
    let v_bytes = value.to_le_bytes();
    poseidon2_hash_domain(b"NOTE_V1", &[domain, &v_bytes, rho, recipient])
}

fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
    poseidon2_hash_domain(b"PRF_NF_V1", &[domain, nf_key, rho])
}

fn root_from_path(leaf: &Hash32, pos: u64, siblings: &[Hash32], depth: u32) -> Hash32 {
    let mut cur = *leaf;
    let mut idx = pos;
    for (lvl, sib) in (0..depth).zip(siblings.iter()) {
        cur = if (idx & 1) == 0 {
            mt_combine(lvl as u8, &cur, sib)
        } else {
            mt_combine(lvl as u8, sib, &cur)
        };
        idx >>= 1;
    }
    cur
}

// ---- Entry point ----

const MAX_ARGS: usize = 128;
const MAX_BUF: usize = 16 * 1024;
const MAX_DEPTH: usize = 64;

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    let mut argc: u32 = 0;
    let mut buf_len: u32 = 0;
    if args_sizes_get(&mut argc, &mut buf_len) != 0 {
        proc_exit(71);
    }
    if (argc as usize) > MAX_ARGS || (buf_len as usize) > MAX_BUF {
        proc_exit(71);
    }

    let mut ptrs: [*mut u8; MAX_ARGS] = [core::ptr::null_mut(); MAX_ARGS];
    let mut buf: [u8; MAX_BUF] = [0; MAX_BUF];
    if args_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) != 0 {
        proc_exit(71);
    }

    // Helper to read arg i into a small stack buffer
    let mut tmp = [0u8; 256];

    // 1) domain
    let n = read_cstr(ptrs[1] as *const u8, &mut tmp);
    let domain = match parse_hex32(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 2) value (u128)
    let n = read_cstr(ptrs[2] as *const u8, &mut tmp);
    let value = match parse_u128_dec(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };
    // STRICT: Enforce value fits in u64 to match on-chain bank amount type
    if value > u64::MAX as u128 {
        proc_exit(71);
    }

    // 3) rho (32)
    let n = read_cstr(ptrs[3] as *const u8, &mut tmp);
    let rho = match parse_hex32(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 4) recipient (32)
    let n = read_cstr(ptrs[4] as *const u8, &mut tmp);
    let recipient = match parse_hex32(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 5) nf_key (32) [PRIVATE]
    let n = read_cstr(ptrs[5] as *const u8, &mut tmp);
    let nf_key = match parse_hex32(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 6) pos (u64)
    let n = read_cstr(ptrs[6] as *const u8, &mut tmp);
    let pos = match parse_u64_dec(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 7) depth (u32)
    let n = read_cstr(ptrs[7] as *const u8, &mut tmp);
    let depth_u32 = match parse_u64_dec(&tmp[..n]) {
        Some(x) => x as u32,
        None => proc_exit(71),
    };
    if depth_u32 as usize > MAX_DEPTH {
        proc_exit(71);
    }
    let depth = depth_u32 as usize;
    
    // STRICT: Check argc is exactly 11 + depth (expected argument count)
    let expected_argc = 11u32 + depth_u32;
    if argc != expected_argc {
        proc_exit(71);
    }
    
    // STRICT: Check position is within valid range for this depth
    if pos >= (1u64 << depth_u32) {
        proc_exit(71);
    }

    // 8) siblings[0..depth] (each 32) [PRIVATE]
    let mut siblings: [Hash32; MAX_DEPTH] = [[0u8; 32]; MAX_DEPTH];
    for i in 0..depth {
        let n = read_cstr(ptrs[8 + i] as *const u8, &mut tmp);
        siblings[i] = match parse_hex32(&tmp[..n]) {
            Some(x) => x,
            None => proc_exit(71),
        };
    }

    // 9) anchor_root (32)
    let n = read_cstr(ptrs[8 + depth] as *const u8, &mut tmp);
    let anchor_arg = match parse_hex32(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 10) nullifier (32)
    let n = read_cstr(ptrs[9 + depth] as *const u8, &mut tmp);
    let nullifier_arg = match parse_hex32(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };

    // 11) withdraw_amount (u128)
    let n = read_cstr(ptrs[10 + depth] as *const u8, &mut tmp);
    let withdraw_amount = match parse_u128_dec(&tmp[..n]) {
        Some(x) => x,
        None => proc_exit(71),
    };
    // STRICT: Enforce withdraw_amount fits in u64 to match on-chain bank amount type
    if withdraw_amount > u64::MAX as u128 {
        proc_exit(71);
    }

    // ---- Compute leaf commitment ----
    let cm = note_commitment(&domain, value, &rho, &recipient);

    // ---- Recompute root from path ----
    let anchor_computed = root_from_path(&cm, pos, &siblings[..depth], depth_u32);

    // Assert anchor matches argument
    assert_one(eq_bytes(&anchor_computed, &anchor_arg) as i32);

    // ---- Compute PRF nullifier ----
    let nf = nullifier(&domain, &nf_key, &rho);

    // Assert nullifier matches argument
    assert_one(eq_bytes(&nf, &nullifier_arg) as i32);

    // ---- Solvency bound: cannot withdraw more than the note value ----
    assert_one((withdraw_amount <= value) as i32);

    // Success
    proc_exit(0)
}

