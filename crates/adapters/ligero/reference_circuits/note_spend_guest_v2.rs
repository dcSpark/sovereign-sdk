/*
 * Rust Guest Program: Note Spend Verifier for Midnight Privacy Pool (outputs + balance)
 *
 * NOTE: DEPOSIT is implemented as a separate, cheaper guest program:
 *   - `utils/circuits/note-deposit` → `utils/circuits/bins/note_deposit_guest.wasm`
 * This guest verifies spends (TRANSFER / WITHDRAW) of an existing note.
 *
 * Verifies a join-split spend with up to 4 shielded inputs and up to 2 shielded outputs:
 *   1) For each input: Merkle root (anchor) from note commitment + auth path
 *   2) For each input: PRF-based nullifier: Poseidon2("PRF_NF_V1" || domain || nf_key || rho)
 *   3) All inputs owned by the same spend key (`spend_sk`)
 *   4) All input nullifiers are distinct (PUBLIC check)
 *   5) Output note commitments (0..=2):
 *        cm_out = Poseidon2("NOTE_V2" || domain || value || rho || recipient || sender_id)
 *      where:
 *        recipient = Poseidon2("ADDR_V2" || domain || pk_spend || pk_ivk)
 *        sender_id = owner_addr (derived from the same spend_sk)
 *   6) Balance: sum(input_values) == withdraw_amount + sum(output_values)
 *
 * =============================================================================
 * BUSINESS REQUIREMENTS - Privacy Pool Transaction Types
 * =============================================================================
 *
 * This circuit supports three transaction types in a shielded payment system:
 *
 * ┌─────────────────────────────────────────────────────────────────────────────┐
 * │ DEPOSIT - Enter the privacy pool                                            │
 * │                                                                             │
 * │   Public:  value (input amount)                                             │
 * │   Private: recipient (who receives the shielded note)                       │
 * │                                                                             │
 * │   Use case: User deposits 100 tokens from their public address into the     │
 * │   shielded pool. Everyone sees the deposit amount and source, but the       │
 * │   recipient's shielded address is hidden.                                   │
 * │   NOTE: transparent origin binding is outside this proof.                   │
 * │                                                                             │
 * │   Circuit config: n_out=1, withdraw_amount=0                                │
 * │   The input comes from a transparent source (not a spent note).             │
 * └─────────────────────────────────────────────────────────────────────────────┘
 *
 * ┌─────────────────────────────────────────────────────────────────────────────┐
 * │ TRANSFER - Fully private transaction within the pool                        │
 * │                                                                             │
 * │   Public:  anchor (state root), nullifier (prevents double-spend),          │
 * │            cm_out (output commitments)                                      │
 * │   Private: value, origin (which note is spent), recipient                   │
 * │                                                                             │
 * │   Use case: Alice sends 50 tokens to Bob. Observers see that *some*         │
 * │   transaction occurred (nullifier published, new commitments added),        │
 * │   but cannot determine the amount, sender, or recipient.                    │
 * │                                                                             │
 * │   Circuit config: n_out=1 or 2, withdraw_amount=0                           │
 * │   Can have 2 outputs for change (e.g., spend 100, send 50, keep 50).        │
 * └─────────────────────────────────────────────────────────────────────────────┘
 *
 * ┌─────────────────────────────────────────────────────────────────────────────┐
 * │ WITHDRAW - Exit the privacy pool                                            │
 * │                                                                             │
 * │   Public:  withdraw_amount (value leaving pool), withdraw_to (destination), │
 * │            anchor, nullifier, cm_out (change commitment if any)             │
 * │   Private: input value, origin (which note), change value                   │
 * │                                                                             │
 * │   Use case: User withdraws 30 tokens to their public address. Observers     │
 * │   see the withdrawal amount and destination, but don't know which note      │
 * │   was spent or the original balance (change is re-shielded).                │
 * │                                                                             │
 * │   Circuit config: n_out=0 or 1, withdraw_amount>0                           │
 * │   - n_out=0: full withdrawal (entire note value exits)                      │
 * │   - n_out=1: partial withdrawal (change goes to shielded output)            │
 * │                                                                             │
 * │   Balance constraint: input_value = withdraw_amount + sum(output_values)    │
 * └─────────────────────────────────────────────────────────────────────────────┘
 *
 * =============================================================================
 *
 * ARGUMENT LAYOUT (WASI args_get, 1-indexed):
 * ============================================
 * Encoding:
 *   - 32-byte values are passed as `0x...` ASCII hex (C-string) by the WebGPU binaries.
 *     The guest decodes them (and also accepts a raw 32-byte fast path when available).
 *   - Integer values are passed as raw little-endian i64 bytes (8 bytes).
 *
 * Address format upgrade: recipients are derived using incoming-view keys:
 *   recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)
 *
 * Header arguments:
 *   [1]  domain        — 32 bytes (PUBLIC)
 *   [2]  spend_sk      — 32 bytes (PRIVATE) - owner spend key for ALL inputs
 *   [3]  pk_ivk_owner  — 32 bytes (PRIVATE) - owner's incoming-view pubkey (derived off-chain)
 *   [4]  depth         — i64 (PUBLIC; shared depth)
 *   [5]  anchor        — 32 bytes (PUBLIC; shared Merkle root)
 *   [6]  n_in          — i64 (PUBLIC; number of inputs in {1..=4})
 *
 * For each input i in [0..n_in):
 *   value_in_i         — i64            [PRIVATE]
 *   rho_in_i           — 32 bytes       [PRIVATE]
 *   sender_id_in_i     — 32 bytes       [PRIVATE] (NOTE_V2 leaf binding)
 *   pos_i              — i64            [PRIVATE] (leaf position; bits are derived in-circuit)
 *   siblings_i[k]      — depth × 32 bytes [PRIVATE]
 *   nullifier_i        — 32 bytes       [PUBLIC; must equal computed]
 *
 * Then:
 *   withdraw_amount    — i64 (PUBLIC)
 *   withdraw_to        — 32 bytes (PUBLIC; transparent recipient/destination, bound to the proof)
 *   n_out              — i64 (PUBLIC; in {0,1,2})
 *
 * For each output j in [0..n_out):
 *   value_out_j        — i64            [PRIVATE]
 *   rho_out_j          — 32 bytes       [PRIVATE]
 *   pk_spend_out_j     — 32 bytes       [PRIVATE]
 *   pk_ivk_out_j       — 32 bytes       [PRIVATE]
 *   cm_out_j           — 32 bytes       [PUBLIC; must equal computed]
 *
 * Then:
 *   inv_enforce        — 32 bytes [PRIVATE] BN254 Fr inverse witness used to enforce:
 *     - all input/output values are non-zero
 *     - output rhos differ from all input rhos
 *     - (if n_out==2) output rhos are distinct
 *
 * Blacklist checks (bucketed non-membership + Merkle membership):
 *   blacklist_root                   — 32 bytes [PUBLIC]
 *   For each checked id:
 *     bl_bucket_entries              — BL_BUCKET_SIZE × 32 bytes [PRIVATE]
 *     bl_bucket_inv                  — 32 bytes [PRIVATE] BN254 Fr inverse witness for in-bucket non-membership
 *     bl_siblings[k]                 — BL_DEPTH × 32 bytes [PRIVATE]
 *
 * Checked ids:
 *   - sender_id always
 *   - pay recipient only for TRANSFER (withdraw_amount == 0)
 *
 * Expected argc (no viewers) =
 *   1 + 6 + n_in*(5 + depth) + 3 + 5*n_out + 1(inv_enforce)
 *     + 1(blacklist_root) + bl_checks*(BL_BUCKET_SIZE + 1 + BL_DEPTH)
 *   where bl_checks = 1 + (withdraw_amount == 0 ? 1 : 0)
 * (argc includes argv[0]).
 *
 * --- Level B: Viewer Attestations (optional; appended after blacklist args) ---
 *
 *   n_viewers          — i64 (PUBLIC; number of viewers, 0..=MAX_VIEWERS)
 *
 *   For each viewer v in [0..n_viewers):
 *     fvk_commit_v      — 32 bytes (PUBLIC)  = H("FVK_COMMIT_V1" || fvk_v)
 *     fvk_v             — 32 bytes (PRIVATE) viewer key material (NOT required for verification)
 *
 *     For each output j in [0..n_out):
 *       ct_hash_v_j     — 32 bytes (PUBLIC)  = H("CT_HASH_V1" || ct_v_j)
 *       mac_v_j         — 32 bytes (PUBLIC)  = H("VIEW_MAC_V1" || k_v_j || cm_out_j || ct_hash_v_j)
 *       where plaintext includes the spent input commitments for traceability:
 *         pt_v_j = [ domain | value | rho | recipient | sender_id | cm_in_0 | ... | cm_in_{MAX_INS-1} ]
 *       (`cm_in_i` are padded with 0x00..00 for i >= n_in)
 *
 * Expected argc (with viewers) =
 *   expected_base + 1(n_viewers) + n_viewers * (1 + 1 + 2*n_out)
 *
 * Notes:
 *   - The verifier MUST NOT require the real `fvk_v`. Only `fvk_commit_v` is public.
 *   - Ensure your runner marks each `fvk_v` argv index as PRIVATE (witness) so verification
 *     only needs the public commitments and digests.
 *
 * SECURITY NOTES:
 *   1) All validation paths inject UNSAT constraints before exit (hard_fail)
 *   2) Balance check uses field-level constraint, not runtime boolean comparison
 *   3) Merkle path uses field-level MUX to avoid witness-dependent constraints
 *
 * Hashing uses Ligetron's Poseidon2 via bn254fr host functions (Ligero-compatible).
 *
 * Viewer plaintexts (Level B):
 *   [ domain | value | rho | recipient | sender_id | cm_in_0 | ... | cm_in_{MAX_INS-1} ]
 *
 */

// =============================================================================
// Ligetron SDK requires std - the heavy Poseidon2 computation is done via
// host functions anyway, so std overhead is minimal.
// =============================================================================

// Ligetron SDK imports
use ligetron::api::{get_args, ArgHolder};
use ligetron::bn254fr::{addmod_checked, mulmod_checked, submod_checked, Bn254Fr};
use ligetron::poseidon2::{poseidon2_hash_bytes, Poseidon2Context};

/// Exit the program with the given code.
fn exit_with_code(code: i32) -> ! {
    std::process::exit(code)
}

/// Conditional exit with detailed error codes in diagnostics mode.
/// In production (no diagnostics feature), all failures exit with code 71.
///
/// Error code conventions (diagnostics mode):
///   70-79: Argument parsing errors
///   80-89: Constraint verification failures
///   90-99: Viewer attestation errors

#[cfg(feature = "diagnostics")]
fn fail_with_code(code: u32) -> ! {
    exit_with_code(code as i32)
}

#[cfg(not(feature = "diagnostics"))]
fn fail_with_code(_code: u32) -> ! {
    exit_with_code(71)
}

/// Hard failure that injects an UNSAT constraint before exiting.
///
/// SECURITY: This is critical for soundness! Without the UNSAT constraint,
/// a malicious prover could trigger a failure path and still get a valid proof
/// for a "truncated" circuit (if the zkVM doesn't enforce exit code checks).
///
/// We do this by forcing the same witness element to equal two different public constants.
#[inline(always)]
fn hard_fail(code: u32) -> ! {
    // Force UNSAT: x == 0 AND x == 1.
    //
    // IMPORTANT: `Bn254Fr::new()` is a witness variable; using `assert_equal()` against a value
    // created via `from_u32()` is satisfiable unless the value is bound as a public constant.
    let x = Bn254Fr::new();
    Bn254Fr::assert_equal_u64(&x, 0);
    Bn254Fr::assert_equal_u64(&x, 1);
    fail_with_code(code)
}

type Hash32 = [u8; 32];

// =============================================================================
// Ligetron-compatible Poseidon2Core shim
// Uses Ligetron's Poseidon2 implementation via bn254fr host functions.
// Uses Ligetron's Poseidon2 implementation via bn254fr host functions.
// =============================================================================

struct Poseidon2Core;

impl Poseidon2Core {
    #[inline(always)]
    pub fn new() -> Self {
        Self
    }

    /// Return Poseidon2 digest as a field element (constraint-friendly).
    #[inline(always)]
    pub fn hash_padded_fr(&self, preimage: &[u8]) -> Bn254Fr {
        poseidon2_hash_bytes(preimage)
    }

    /// Return Poseidon2 digest as 32-byte BE (for Merkle/preimage composition).
    #[inline(always)]
    pub fn hash_padded(&self, preimage: &[u8]) -> Hash32 {
        let digest = self.hash_padded_fr(preimage);
        bn254fr_to_hash32(&digest)
    }

    /// Return Poseidon2 digest as a field element from byte field elements.
    #[inline(always)]
    pub fn hash_padded_fr_bytes(&self, preimage: &[Bn254Fr]) -> Bn254Fr {
        let mut ctx = Poseidon2Context::new();
        let mut offset = 0usize;

        // Process full 31-byte blocks.
        while preimage.len().saturating_sub(offset) >= 31 {
            let chunk_fr = bytes_to_fr_be(&preimage[offset..offset + 31]);
            ctx.digest_update(&chunk_fr);
            offset += 31;
        }

        // Final block: remaining bytes + 0x80 + zero padding.
        let mut block: Vec<Bn254Fr> = Vec::with_capacity(31);
        block.extend_from_slice(&preimage[offset..]);
        block.push(Bn254Fr::from_u32(0x80));
        while block.len() < 31 {
            block.push(Bn254Fr::from_u32(0));
        }
        let last_fr = bytes_to_fr_be(&block);
        ctx.digest_update(&last_fr);

        ctx.digest_final_no_pad()
    }

    /// Return Poseidon2 digest as 32-byte BE (field bytes) from byte field elements.
    #[inline(always)]
    pub fn hash_padded_bytes_frs(&self, preimage: &[Bn254Fr]) -> [Bn254Fr; 32] {
        let digest = self.hash_padded_fr_bytes(preimage);
        fr_to_bytes_be_bits(&digest)
    }
}

/// Convert a Bn254Fr field element to a 32-byte hash.
/// Uses big-endian byte order for compatibility with existing test vectors.
#[inline(always)]
fn bn254fr_to_hash32(x: &Bn254Fr) -> Hash32 {
    x.to_bytes_be()
}

/// Convert a 32-byte big-endian hash to a Bn254Fr field element.
/// Uses hex encoding to avoid value-dependent parsing.
#[inline(always)]
fn bn254fr_from_hash32_be(h: &Hash32) -> Bn254Fr {
    let mut result = Bn254Fr::new();
    result.set_bytes_big(h);
    result
}

#[inline(always)]
fn assert_hash32_eq(a: &Hash32, b: &Hash32) {
    let a_fr = bn254fr_from_hash32_be(a);
    let b_fr = bn254fr_from_hash32_be(b);
    Bn254Fr::assert_equal(&a_fr, &b_fr);
}

/// Assert that a computed field element equals an expected 32-byte digest (public).
#[inline(always)]
fn assert_fr_eq_hash32(computed: &Bn254Fr, expected_be: &Hash32) {
    // Bind the expected digest bytes into the statement as a constant and constrain `computed` to it.
    //
    // NOTE: This is critical for soundness when the verifier reconstructs the constraint system
    // without evaluating private inputs: parsing bytes into a field element via `set_bytes_big`
    // does *not* by itself constrain the value to equal those bytes.
    Bn254Fr::assert_equal_bytes_be(computed, expected_be);
}

// ============================================================================
// BYTE/FR HELPERS (Constraint-friendly)
// ============================================================================

/// Pack up to 31 byte field elements into a single field element (big-endian).
#[inline(always)]
fn bytes_to_fr_be(bytes: &[Bn254Fr]) -> Bn254Fr {
    let mut acc = Bn254Fr::from_u32(0);
    let base = Bn254Fr::from_u32(256);
    for b in bytes {
        let mut tmp = Bn254Fr::new();
        mulmod_checked(&mut tmp, &acc, &base);
        addmod_checked(&mut acc, &tmp, b);
    }
    acc
}

#[inline(always)]
fn bits_to_byte_fr(bits: &[Bn254Fr; 8]) -> Bn254Fr {
    let pow2: [Bn254Fr; 8] = std::array::from_fn(|i| Bn254Fr::from_u32(1u32 << i));
    let mut out = Bn254Fr::from_u32(0);
    for i in 0..8 {
        let mut term = Bn254Fr::new();
        mulmod_checked(&mut term, &bits[i], &pow2[i]);
        out.addmod_checked(&term);
    }
    out
}

/// Convert a field element into 32 byte field elements (big-endian) via bit decomposition.
#[inline(always)]
fn fr_to_bytes_be_bits(x: &Bn254Fr) -> [Bn254Fr; 32] {
    let mut out: [Bn254Fr; 32] = std::array::from_fn(|_| Bn254Fr::new());
    let bits = x.to_bits(254); // LSB-first
    let zero = Bn254Fr::from_u32(0);

    for i in 0..32 {
        let mut byte_bits: [Bn254Fr; 8] = std::array::from_fn(|_| Bn254Fr::new());
        for j in 0..8 {
            let bit_idx = i * 8 + j;
            if bit_idx < bits.len() {
                byte_bits[j] = bits[bit_idx].clone();
            } else {
                byte_bits[j] = zero.clone();
            }
        }
        let byte_fr = bits_to_byte_fr(&byte_bits);
        out[31 - i] = byte_fr;
    }
    out
}

/// Convert a 32-byte array into field-byte elements (byte value only).
#[inline(always)]
fn hash32_to_fr_bytes(h: &Hash32) -> [Bn254Fr; 32] {
    let mut out: [Bn254Fr; 32] = std::array::from_fn(|_| Bn254Fr::new());
    for i in 0..32 {
        out[i].set_bytes_big(&h[i..i + 1]);
    }
    out
}

/// Convert a private 32-byte array into field-byte elements and enforce 8-bit range.
#[inline(always)]
fn hash32_to_fr_bytes_range_checked(h: &Hash32) -> [Bn254Fr; 32] {
    let out = hash32_to_fr_bytes(h);
    for i in 0..32 {
        let _ = out[i].to_bits(8);
    }
    out
}

/// Convert a public 32-byte array into field-byte elements and bind each byte.
#[inline(always)]
fn hash32_to_fr_bytes_constrained(h: &Hash32) -> [Bn254Fr; 32] {
    let out = hash32_to_fr_bytes(h);
    for i in 0..32 {
        Bn254Fr::assert_equal_bytes_be(&out[i], &h[i..i + 1]);
    }
    out
}

/// Convert a NOTE_PLAIN_LEN-byte array into field-byte elements (byte value only).
#[inline(always)]
fn bytes_note_plain_to_fr_bytes(bytes: &[u8; NOTE_PLAIN_LEN]) -> [Bn254Fr; NOTE_PLAIN_LEN] {
    let mut out: [Bn254Fr; NOTE_PLAIN_LEN] = std::array::from_fn(|_| Bn254Fr::new());
    for i in 0..NOTE_PLAIN_LEN {
        out[i].set_bytes_big(&bytes[i..i + 1]);
    }
    out
}

/// XOR two byte field elements using bit-level constraints.
#[inline(always)]
fn xor_byte_fr(a: &Bn254Fr, b: &Bn254Fr) -> Bn254Fr {
    let a_bits = a.to_bits(8);
    let b_bits = b.to_bits(8);
    let two = Bn254Fr::from_u32(2);
    let mut out_bits: [Bn254Fr; 8] = std::array::from_fn(|_| Bn254Fr::new());

    for i in 0..8 {
        let mut ab = Bn254Fr::new();
        mulmod_checked(&mut ab, &a_bits[i], &b_bits[i]);
        let mut sum = Bn254Fr::new();
        addmod_checked(&mut sum, &a_bits[i], &b_bits[i]);
        let mut two_ab = Bn254Fr::new();
        mulmod_checked(&mut two_ab, &ab, &two);
        let mut out = Bn254Fr::new();
        submod_checked(&mut out, &sum, &two_ab);
        out_bits[i] = out;
    }

    bits_to_byte_fr(&out_bits)
}

// ============================================================================
// MERKLE PATH (FIELD-LEVEL MUX)
// Uses arithmetic selection with position bits derived from a private u64 `pos`.
// ============================================================================

// ============================================================================
// OPTIMIZED: All values stored as u64 (not u128) to avoid expensive 128-bit ops.
// Values are encoded to 16-byte LE with zero-extension for protocol compatibility.
// ============================================================================

// ============================================================================
// ARGUMENT HELPERS: Read typed args from ArgHolder.
//
// The WebGPU prover/verifier pass `hex` args as `0x...` ASCII hex (C-string),
// so we decode them here in a branchless way (and also accept a raw 32-byte fast path).
// ============================================================================

/// Convert an ASCII hex character into its 4-bit value.
///
/// This implementation is branchless and uses no lookup tables, so it does not introduce
/// secret-dependent control flow or secret-indexed memory access.
///
/// Invalid characters map to 0.
#[inline(always)]
fn hex_char_to_nibble(c: u8) -> u8 {
    // '0'..'9'
    let d = c.wrapping_sub(b'0');
    let md = (0u8).wrapping_sub((d <= 9) as u8);

    // 'a'..'f'
    let a = c.wrapping_sub(b'a');
    let ma = (0u8).wrapping_sub((a <= 5) as u8);

    // 'A'..'F'
    #[allow(non_snake_case)]
    let A = c.wrapping_sub(b'A');
    #[allow(non_snake_case)]
    let mA = (0u8).wrapping_sub((A <= 5) as u8);

    (d & md) | (a.wrapping_add(10) & ma) | (A.wrapping_add(10) & mA)
}

#[inline(always)]
fn read_hash32(args: &ArgHolder, index: usize) -> Hash32 {
    let bytes = args.get_as_bytes(index);
    let mut out = [0u8; 32];
    // Fast-path: some hosts may pass raw 32 bytes directly.
    if bytes.len() == 32 {
        out.copy_from_slice(bytes);
        return out;
    }

    // Common path: Ligero passes `hex` args as a C-string `0x...` with a trailing `\0`.
    // Expected length: 2 ("0x") + 64 hex chars + 1 NUL = 67 bytes.
    let hex_bytes = if bytes.len() == 67 {
        &bytes[2..66]
    } else if bytes.len() == 66 {
        // Same without a trailing NUL.
        &bytes[2..66]
    } else {
        #[cfg(feature = "diagnostics")]
        {
            eprintln!(
                "read_hash32: idx={} len={} (unexpected)",
                index,
                bytes.len()
            );
            eprintln!(
                "read_hash32: first_bytes={:02x?}",
                &bytes[..bytes.len().min(16)]
            );
        }
        hard_fail(70);
    };

    // Decode ASCII hex -> 32 bytes, without secret-dependent branching or table lookups.
    let mut i = 0usize;
    while i < 32 {
        let hi = hex_char_to_nibble(hex_bytes[2 * i]);
        let lo = hex_char_to_nibble(hex_bytes[2 * i + 1]);
        out[i] = (hi << 4) | lo;
        i += 1;
    }

    out
}

/// Read a non-negative i64 as u64, failing with error code if negative.
#[inline(always)]
fn read_u64(args: &ArgHolder, index: usize, fail_code: u32) -> u64 {
    let v = args.get_as_int(index);
    if v < 0 {
        hard_fail(fail_code);
    }
    v as u64
}

/// Read a u32 from an i64 arg, validating range.
#[inline(always)]
fn read_u32(args: &ArgHolder, index: usize, fail_code: u32) -> u32 {
    let v = args.get_as_int(index);
    if v < 0 || v > u32::MAX as i64 {
        hard_fail(fail_code);
    }
    v as u32
}

// (intentionally no public hash -> field helper; use read_hash32 + assert_fr_eq_hash32)

// ============================================================================
// OPTIMIZED HASH FUNCTIONS: Fixed-size buffers, single hasher instance
// Each hash type has a dedicated function with exact buffer size.
// ============================================================================

// Fixed buffer sizes for each hash type (tag + data)
const MT_NODE_BUF_LEN: usize = 10 + 1 + 32 + 32; // "MT_NODE_V1" + lvl + left + right = 75
const NOTE_CM_BUF_LEN: usize = 7 + 32 + 16 + 32 + 32 + 32; // "NOTE_V2" + domain + value + rho + recipient + sender_id = 151
const PRF_NF_BUF_LEN: usize = 9 + 32 + 32 + 32; // "PRF_NF_V1" + domain + nf_key + rho = 105
const PK_BUF_LEN: usize = 5 + 32; // "PK_V1" + spend_sk = 37
const ADDR_BUF_LEN: usize = 7 + 32 + 32 + 32; // "ADDR_V2" + domain + pk_spend + pk_ivk = 103
const NFKEY_BUF_LEN: usize = 8 + 32 + 32; // "NFKEY_V1" + domain + spend_sk = 72
const FVK_COMMIT_BUF_LEN: usize = 13 + 32; // "FVK_COMMIT_V1" + fvk = 45
const VIEW_KDF_BUF_LEN: usize = 11 + 32 + 32; // "VIEW_KDF_V1" + fvk + cm = 75
const VIEW_STREAM_BUF_LEN: usize = 14 + 32 + 4; // "VIEW_STREAM_V1" + k + ctr = 50
const CT_HASH_BUF_LEN: usize = 10 + NOTE_PLAIN_LEN; // "CT_HASH_V1" + ct = 10 + NOTE_PLAIN_LEN
const VIEW_MAC_BUF_LEN: usize = 11 + 32 + 32 + 32; // "VIEW_MAC_V1" + k + cm + ct_hash = 107

/// Merkle tree node hash: H("MT_NODE_V1" || lvl || left || right)
/// Fixed 75-byte preimage.

fn mt_combine(h: &Poseidon2Core, level: u8, left: &Hash32, right: &Hash32) -> Bn254Fr {
    let mut buf = [0u8; MT_NODE_BUF_LEN];
    buf[..10].copy_from_slice(b"MT_NODE_V1");
    buf[10] = level;
    buf[11..43].copy_from_slice(left);
    buf[43..75].copy_from_slice(right);
    h.hash_padded_fr(&buf)
}

/// Note commitment: H("NOTE_V2" || domain || value_16 || rho || recipient || sender_id)
/// Fixed 151-byte preimage. Value is u64 zero-extended to 16 bytes.
///
/// `sender_id` is the attested sender identity bound into the commitment (not just viewer plaintext).
fn note_commitment_fr(
    h: &Poseidon2Core,
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> Bn254Fr {
    let mut buf = [0u8; NOTE_CM_BUF_LEN];
    buf[..7].copy_from_slice(b"NOTE_V2");
    buf[7..39].copy_from_slice(domain);
    // Encode value as 16-byte LE (zero-extended from u64)
    buf[39..47].copy_from_slice(&value.to_le_bytes());
    // buf[47..55] already zero from initialization (zero-extension)
    buf[55..87].copy_from_slice(rho);
    buf[87..119].copy_from_slice(recipient);
    buf[119..151].copy_from_slice(sender_id);
    h.hash_padded_fr(&buf)
}

/// Nullifier: H("PRF_NF_V1" || domain || nf_key || rho)
/// Fixed 105-byte preimage.

fn nullifier_fr(h: &Poseidon2Core, domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Bn254Fr {
    let mut buf = [0u8; PRF_NF_BUF_LEN];
    buf[..9].copy_from_slice(b"PRF_NF_V1");
    buf[9..41].copy_from_slice(domain);
    buf[41..73].copy_from_slice(nf_key);
    buf[73..105].copy_from_slice(rho);
    h.hash_padded_fr(&buf)
}

/// pk = H("PK_V1" || spend_sk)
/// Fixed 37-byte preimage.

fn pk_from_sk(h: &Poseidon2Core, spend_sk: &Hash32) -> Hash32 {
    let mut buf = [0u8; PK_BUF_LEN];
    buf[..5].copy_from_slice(b"PK_V1");
    buf[5..37].copy_from_slice(spend_sk);
    h.hash_padded(&buf)
}

/// recipient_addr = H("ADDR_V2" || domain || pk_spend || pk_ivk)
/// Fixed 103-byte preimage.
#[inline(always)]
fn recipient_from_pk(
    h: &Poseidon2Core,
    domain: &Hash32,
    pk_spend: &Hash32,
    pk_ivk: &Hash32,
) -> Hash32 {
    let mut buf = [0u8; ADDR_BUF_LEN];
    buf[..7].copy_from_slice(b"ADDR_V2");
    buf[7..39].copy_from_slice(domain);
    buf[39..71].copy_from_slice(pk_spend);
    buf[71..103].copy_from_slice(pk_ivk);
    h.hash_padded(&buf)
}

/// nf_key = H("NFKEY_V1" || domain || spend_sk)
/// Fixed 72-byte preimage.

fn nf_key_from_sk(h: &Poseidon2Core, domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
    let mut buf = [0u8; NFKEY_BUF_LEN];
    buf[..8].copy_from_slice(b"NFKEY_V1");
    buf[8..40].copy_from_slice(domain);
    buf[40..72].copy_from_slice(spend_sk);
    h.hash_padded(&buf)
}

/// Compute Merkle root using FIELD-LEVEL MUX operations.
/// Position bits are derived from the low bits of `pos` (LSB-first).
///
/// Reads `depth` sibling hashes from `args` starting at `*arg_idx` and advances `*arg_idx`.
fn root_from_path_field_level(
    h: &Poseidon2Core,
    mut cur_fr: Bn254Fr,
    mut pos: u64,
    args: &ArgHolder,
    arg_idx: &mut usize,
    depth: usize,
) -> Bn254Fr {
    if depth == 0 {
        hard_fail(77);
    }

    // Reuse temporaries; this also reduces per-level host overhead.
    let mut left_fr = Bn254Fr::new();
    let mut right_fr = Bn254Fr::new();
    let mut delta = Bn254Fr::new();
    let mut bit_fr = Bn254Fr::new();
    let mut left_bytes = [0u8; 32];
    let mut right_bytes = [0u8; 32];

    let mut lvl = 0usize;
    while lvl < depth {
        let sib = read_hash32(args, *arg_idx);
        *arg_idx += 1;
        let sib_fr = bn254fr_from_hash32_be(&sib);
        bit_fr.set_u32((pos & 1) as u32);

        // 1-mul select:
        // delta = bit * (sib - cur)
        // left  = cur + delta
        // right = sib - delta
        submod_checked(&mut delta, &sib_fr, &cur_fr);
        delta.mulmod_checked(&bit_fr);
        addmod_checked(&mut left_fr, &cur_fr, &delta);
        submod_checked(&mut right_fr, &sib_fr, &delta);

        left_fr.get_bytes_big(&mut left_bytes);
        right_fr.get_bytes_big(&mut right_bytes);

        // Compute hash using byte preimage.
        cur_fr = mt_combine(h, lvl as u8, &left_bytes, &right_bytes);
        pos >>= 1;
        lvl += 1;
    }

    cur_fr
}

// === Level B: Viewer Attestation Functions ===

/// FVK commitment: H("FVK_COMMIT_V1" || fvk)
/// Fixed 45-byte preimage.
fn fvk_commit_fr(h: &Poseidon2Core, fvk: &[Bn254Fr; 32]) -> Bn254Fr {
    let mut buf: Vec<Bn254Fr> = Vec::with_capacity(FVK_COMMIT_BUF_LEN);
    for b in b"FVK_COMMIT_V1" {
        buf.push(Bn254Fr::from_u32(*b as u32));
    }
    buf.extend_from_slice(fvk);
    h.hash_padded_fr_bytes(&buf)
}

/// View KDF: H("VIEW_KDF_V1" || fvk || cm)
/// Fixed 75-byte preimage.
fn view_kdf(h: &Poseidon2Core, fvk: &[Bn254Fr; 32], cm: &[Bn254Fr; 32]) -> [Bn254Fr; 32] {
    let mut buf: Vec<Bn254Fr> = Vec::with_capacity(VIEW_KDF_BUF_LEN);
    for b in b"VIEW_KDF_V1" {
        buf.push(Bn254Fr::from_u32(*b as u32));
    }
    buf.extend_from_slice(fvk);
    buf.extend_from_slice(cm);
    h.hash_padded_bytes_frs(&buf)
}

/// Stream block: H("VIEW_STREAM_V1" || k || ctr)
/// Fixed 50-byte preimage.
fn stream_block(h: &Poseidon2Core, k: &[Bn254Fr; 32], ctr: u32) -> [Bn254Fr; 32] {
    let mut buf: Vec<Bn254Fr> = Vec::with_capacity(VIEW_STREAM_BUF_LEN);
    for b in b"VIEW_STREAM_V1" {
        buf.push(Bn254Fr::from_u32(*b as u32));
    }
    buf.extend_from_slice(k);
    for b in ctr.to_le_bytes() {
        buf.push(Bn254Fr::from_u32(b as u32));
    }
    h.hash_padded_bytes_frs(&buf)
}

/// Stream XOR encrypt for `NOTE_PLAIN_LEN` bytes.
/// Optimized: 9 hash calls for NOTE_PLAIN_LEN=272 (8 full blocks + 16-byte remainder).
fn stream_xor_encrypt_note_plain(
    h: &Poseidon2Core,
    k: &[Bn254Fr; 32],
    pt: &[Bn254Fr; NOTE_PLAIN_LEN],
    ct_out: &mut [Bn254Fr; NOTE_PLAIN_LEN],
) {
    let mut ctr = 0u32;
    while (ctr as usize) * 32 < NOTE_PLAIN_LEN {
        let ks = stream_block(h, k, ctr);
        let off = (ctr as usize) * 32;
        let take = core::cmp::min(32, NOTE_PLAIN_LEN - off);
        for i in 0..take {
            ct_out[off + i] = xor_byte_fr(&pt[off + i], &ks[i]);
        }
        ctr += 1;
    }
}

/// Ciphertext hash: H("CT_HASH_V1" || ct)
/// Fixed (10 + NOTE_PLAIN_LEN)-byte preimage for NOTE_PLAIN_LEN-byte ciphertext.
fn ct_hash_fr(h: &Poseidon2Core, ct: &[Bn254Fr; NOTE_PLAIN_LEN]) -> Bn254Fr {
    let mut buf: Vec<Bn254Fr> = Vec::with_capacity(CT_HASH_BUF_LEN);
    for b in b"CT_HASH_V1" {
        buf.push(Bn254Fr::from_u32(*b as u32));
    }
    buf.extend_from_slice(ct);
    h.hash_padded_fr_bytes(&buf)
}

/// View MAC: H("VIEW_MAC_V1" || k || cm || ct_hash)
/// Fixed 107-byte preimage.
fn view_mac_fr(
    h: &Poseidon2Core,
    k: &[Bn254Fr; 32],
    cm: &[Bn254Fr; 32],
    ct_h: &[Bn254Fr; 32],
) -> Bn254Fr {
    let mut buf: Vec<Bn254Fr> = Vec::with_capacity(VIEW_MAC_BUF_LEN);
    for b in b"VIEW_MAC_V1" {
        buf.push(Bn254Fr::from_u32(*b as u32));
    }
    buf.extend_from_slice(k);
    buf.extend_from_slice(cm);
    buf.extend_from_slice(ct_h);
    h.hash_padded_fr_bytes(&buf)
}

/// Encode note plaintext for viewer encryption.
/// [ domain(32) | value_le_16 | rho(32) | recipient(32) | sender_id(32) | cm_in[0..MAX_INS) ] => NOTE_PLAIN_LEN bytes
/// Value is u64 zero-extended to 16 bytes.

fn encode_note_plain(
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
    cm_ins: &[Hash32; MAX_INS],
    out: &mut [u8; NOTE_PLAIN_LEN],
) {
    out[0..32].copy_from_slice(domain);
    // Encode value as 16-byte LE (u64 zero-extended to 16 bytes)
    out[32..40].copy_from_slice(&value.to_le_bytes());
    // Explicitly zero the high 8 bytes for self-contained correctness
    // (don't rely on caller to pre-zero the buffer)
    out[40..48].copy_from_slice(&[0u8; 8]);
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
    let mut off = 144usize;
    for cm in cm_ins {
        out[off..off + 32].copy_from_slice(cm);
        off += 32;
    }
}

/// Maximum Merkle tree depth supported by the circuit.
/// Must be ≤ 63 to ensure the bound check `pos >= (1u64 << depth)` is safe
/// (shifting by 64 would overflow a u64).
const MAX_DEPTH: usize = 63;
const BL_DEPTH: usize = 16;
const BL_BUCKET_SIZE: usize = 12;
const BL_BUCKET_TAG_LEN: usize = 12; // "BL_BUCKET_V1"
const BL_BUCKET_BUF_LEN: usize = BL_BUCKET_TAG_LEN + 32 * BL_BUCKET_SIZE;
const MAX_INS: usize = 4;
const MAX_OUTS: usize = 2;
const MAX_VIEWERS: usize = 8;
const NOTE_PLAIN_LEN: usize = 144 + 32 * MAX_INS; // domain + value + rho + recipient + sender_id + MAX_INS commitments

#[inline(always)]
fn bl_bucket_pos_from_id(id: &Hash32) -> u64 {
    // Derive the bucket index from the low BL_DEPTH bits of `id` (LSB-first).
    // This prevents the prover from choosing an arbitrary bucket.
    let mut pos: u64 = 0;
    let mut i = 0usize;
    while i < BL_DEPTH {
        let byte = id[31 - (i / 8)];
        let bit = (byte >> (i % 8)) & 1;
        pos |= (bit as u64) << (i as u32);
        i += 1;
    }
    pos
}

#[inline(always)]
fn bl_bucket_leaf_fr(h: &Poseidon2Core, entries: &[Hash32; BL_BUCKET_SIZE]) -> Bn254Fr {
    let mut buf = [0u8; BL_BUCKET_BUF_LEN];
    buf[..BL_BUCKET_TAG_LEN].copy_from_slice(b"BL_BUCKET_V1");
    let mut i = 0usize;
    while i < BL_BUCKET_SIZE {
        let start = BL_BUCKET_TAG_LEN + 32 * i;
        buf[start..start + 32].copy_from_slice(&entries[i]);
        i += 1;
    }
    h.hash_padded_fr(&buf)
}

#[inline(always)]
fn assert_not_blacklisted_bucket_from_args(
    h: &Poseidon2Core,
    id: &Hash32,
    blacklist_root: &Hash32,
    args: &ArgHolder,
    arg_idx: &mut usize,
) {
    let mut bucket_entries = [[0u8; 32]; BL_BUCKET_SIZE];
    for i in 0..BL_BUCKET_SIZE {
        bucket_entries[i] = read_hash32(args, *arg_idx);
        *arg_idx += 1;
    }
    let inv_bytes = read_hash32(args, *arg_idx);
    *arg_idx += 1;

    let pos = bl_bucket_pos_from_id(id);

    // In-bucket non-membership: prove `id` differs from every entry by supplying inv(product).
    let id_fr = bn254fr_from_hash32_be(id);
    let mut prod = Bn254Fr::from_u32(1);
    let mut delta = Bn254Fr::new();
    for e in bucket_entries.iter() {
        let e_fr = bn254fr_from_hash32_be(e);
        submod_checked(&mut delta, &id_fr, &e_fr);
        prod.mulmod_checked(&delta);
    }
    let inv_fr = bn254fr_from_hash32_be(&inv_bytes);
    prod.mulmod_checked(&inv_fr);
    Bn254Fr::assert_equal_u64(&prod, 1);

    // Bucket membership under `blacklist_root`.
    let leaf_fr = bl_bucket_leaf_fr(h, &bucket_entries);
    let root_fr = root_from_path_field_level(h, leaf_fr, pos, args, arg_idx, BL_DEPTH);
    assert_fr_eq_hash32(&root_fr, blacklist_root);
}

fn main() {
    let args = get_args();
    let argc = args.len() as u32;

    // Create single hasher instance, reuse for all hashes.
    let h = Poseidon2Core::new();

    // Header:
    // [1] domain (PUBLIC)
    // [2] spend_sk (PRIVATE)
    // [3] pk_ivk_owner (PRIVATE)
    // [4] depth (PUBLIC)
    // [5] anchor (PUBLIC)
    // [6] n_in (PUBLIC)
    let domain = read_hash32(&args, 1);
    let spend_sk = read_hash32(&args, 2);
    let pk_ivk_owner = read_hash32(&args, 3);

    let depth_u32 = read_u32(&args, 4, 77);
    if depth_u32 > MAX_DEPTH as u32 {
        hard_fail(77);
    }
    let depth = depth_u32 as usize;

    let anchor_arg = read_hash32(&args, 5);
    let n_in_u32 = read_u32(&args, 6, 78);
    if n_in_u32 == 0 || n_in_u32 > MAX_INS as u32 {
        hard_fail(78);
    }
    let n_in = n_in_u32 as usize;

    // Owner identity (attested): derive recipient(owner) from spend_sk + pk_ivk_owner.
    let pk_spend_owner = pk_from_sk(&h, &spend_sk);
    let recipient_owner = recipient_from_pk(&h, &domain, &pk_spend_owner, &pk_ivk_owner);
    let sender_id = recipient_owner;

    // Shared nf_key (same owner for all inputs).
    let nf_key = nf_key_from_sk(&h, &domain, &spend_sk);

    // Parse inputs.
    let mut arg_idx: usize = 7;
    let mut sum_in: u64 = 0;
    let mut nullifier_args: Vec<Hash32> = Vec::with_capacity(n_in);
    let mut enforce_prod = Bn254Fr::from_u32(1);
    let mut in_rhos_fr: Vec<Bn254Fr> = Vec::with_capacity(n_in);
    struct InPlain {
        v: u64,
        rho: Hash32,
        sender_id_in: Hash32,
    }
    let mut in_plains: Vec<InPlain> = Vec::with_capacity(n_in);

    for _i in 0..n_in {
        // value_in_i [PRIVATE]
        let v_i = read_u64(&args, arg_idx, 72);
        arg_idx += 1;
        sum_in = sum_in.checked_add(v_i).unwrap_or_else(|| hard_fail(86));
        let v_i_fr = Bn254Fr::from_u64(v_i);
        enforce_prod.mulmod_checked(&v_i_fr);

        // rho_in_i [PRIVATE]
        let rho_i = read_hash32(&args, arg_idx);
        arg_idx += 1;
        in_rhos_fr.push(bn254fr_from_hash32_be(&rho_i));

        // sender_id_in_i [PRIVATE] (NOTE_V2 leaf binding)
        let sender_id_in_i = read_hash32(&args, arg_idx);
        arg_idx += 1;
        in_plains.push(InPlain {
            v: v_i,
            rho: rho_i,
            sender_id_in: sender_id_in_i,
        });

        // pos_i [PRIVATE]
        let pos_i = read_u64(&args, arg_idx, 77);
        arg_idx += 1;
        if pos_i >= (1u64 << depth) {
            hard_fail(77);
        }

        // Verify Merkle membership for this input.
        let cm_i_fr =
            note_commitment_fr(&h, &domain, v_i, &rho_i, &recipient_owner, &sender_id_in_i);
        let anchor_i_fr =
            root_from_path_field_level(&h, cm_i_fr, pos_i, &args, &mut arg_idx, depth);
        assert_fr_eq_hash32(&anchor_i_fr, &anchor_arg);

        // nullifier_i [PUBLIC]
        let nullifier_arg_i = read_hash32(&args, arg_idx);
        arg_idx += 1;
        nullifier_args.push(nullifier_arg_i);

        // Verify nullifier for this input.
        let nf_i_fr = nullifier_fr(&h, &domain, &nf_key, &rho_i);
        assert_fr_eq_hash32(&nf_i_fr, &nullifier_args[nullifier_args.len() - 1]);
    }

    // Nullifiers must be distinct within the transaction (PUBLIC check).
    for i in 0..nullifier_args.len() {
        for j in (i + 1)..nullifier_args.len() {
            if nullifier_args[i] == nullifier_args[j] {
                hard_fail(80);
            }
        }
    }

    // withdraw_amount (PUBLIC)
    let withdraw_amount = read_u64(&args, arg_idx, 82);
    arg_idx += 1;

    // withdraw_to (PUBLIC; transparent destination)
    let withdraw_to = read_hash32(&args, arg_idx);
    arg_idx += 1;

    // n_out (PUBLIC)
    let n_out_u32 = read_u32(&args, arg_idx, 83);
    arg_idx += 1;
    if n_out_u32 > MAX_OUTS as u32 {
        hard_fail(83);
    }
    let n_out = n_out_u32 as usize;

    // Shape rules:
    // - withdraw_amount == 0  => n_out ∈ {1,2}
    // - withdraw_amount  > 0  => n_out ∈ {0,1}
    if withdraw_amount == 0 {
        if n_out == 0 {
            hard_fail(87);
        }
        // Transfers have no transparent destination; keep it canonical.
        if withdraw_to != [0u8; 32] {
            hard_fail(93);
        }
    } else if n_out > 1 {
        hard_fail(87);
    } else if withdraw_to == [0u8; 32] {
        // Withdrawing to a zero destination is almost certainly a bug.
        hard_fail(93);
    }

    // Expected argc without viewers:
    //   1 + 6 + n_in*(5 + depth) + 3 + 5*n_out + 1(inv_enforce)
    let per_in = 5u32 + depth_u32;
    let expected_base_no_blacklist =
        1u32 + 6u32 + n_in_u32 * per_in + 3u32 + 5u32 * n_out_u32 + 1u32;
    // Blacklist arguments are appended after inv_enforce:
    //   blacklist_root [PUBLIC]
    //   For each checked id:
    //     bucket_entries[BL_BUCKET_SIZE] [PRIVATE]
    //     bucket_inv                 [PRIVATE]
    //     bucket_siblings[BL_DEPTH]  [PRIVATE]
    let bl_pay_checks = if withdraw_amount == 0 { 1u32 } else { 0u32 };
    let bl_checks = 1u32 + bl_pay_checks;
    let bl_per_check = (BL_BUCKET_SIZE as u32) + 1u32 + (BL_DEPTH as u32);
    let blacklist_extra = 1u32 + bl_checks * bl_per_check;
    let expected_base = expected_base_no_blacklist + blacklist_extra;
    if argc < expected_base {
        hard_fail(84);
    }

    // Parse & verify outputs.
    struct OutPlain {
        v: u64,
        rho: Hash32,
        rcp: Hash32,
        cm: Hash32,
    }
    let mut outs: [OutPlain; MAX_OUTS] = [
        OutPlain {
            v: 0,
            rho: [0; 32],
            rcp: [0; 32],
            cm: [0; 32],
        },
        OutPlain {
            v: 0,
            rho: [0; 32],
            rcp: [0; 32],
            cm: [0; 32],
        },
    ];

    let mut out_sum: u64 = 0;
    let mut out_rhos_fr: Vec<Bn254Fr> = Vec::with_capacity(n_out);
    for j in 0..n_out {
        // value_out_j [PRIVATE]
        let vj = read_u64(&args, arg_idx, 85);
        arg_idx += 1;
        out_sum = out_sum.checked_add(vj).unwrap_or_else(|| hard_fail(86));
        let vj_fr = Bn254Fr::from_u64(vj);
        enforce_prod.mulmod_checked(&vj_fr);

        // rho_out_j [PRIVATE]
        let rho_j = read_hash32(&args, arg_idx);
        arg_idx += 1;
        out_rhos_fr.push(bn254fr_from_hash32_be(&rho_j));

        // pk_spend_out_j [PRIVATE]
        let pk_spend_out_j = read_hash32(&args, arg_idx);
        arg_idx += 1;

        // pk_ivk_out_j [PRIVATE]
        let pk_ivk_out_j = read_hash32(&args, arg_idx);
        arg_idx += 1;

        // recipient is derived from both keys (ADDR_V2).
        let rcp_j = recipient_from_pk(&h, &domain, &pk_spend_out_j, &pk_ivk_out_j);

        // cm_out_j (PUBLIC)
        let cm_arg = read_hash32(&args, arg_idx);
        arg_idx += 1;

        let cm_cmp_fr = note_commitment_fr(&h, &domain, vj, &rho_j, &rcp_j, &sender_id);
        assert_fr_eq_hash32(&cm_cmp_fr, &cm_arg);

        outs[j] = OutPlain {
            v: vj,
            rho: rho_j,
            rcp: rcp_j,
            cm: cm_arg,
        };
    }

    // Enforce protocol shape: change outputs (if present) go back to the sender.
    //
    // This lets us skip blacklist checks for change outputs, cutting blacklist cost roughly in half
    // for withdraws and by ~33% for 2-output transfers.
    if withdraw_amount > 0 {
        if n_out == 1 {
            assert_hash32_eq(&outs[0].rcp, &sender_id);
        }
    } else if n_out == 2 {
        assert_hash32_eq(&outs[1].rcp, &sender_id);
    }

    // Balance: sum_in == withdraw + sum(outputs)
    let _rhs_check = withdraw_amount
        .checked_add(out_sum)
        .unwrap_or_else(|| hard_fail(90));

    let sum_in_fr = Bn254Fr::from_u64(sum_in);
    let withdraw_fr = Bn254Fr::from_u64(withdraw_amount);
    let out_sum_fr = Bn254Fr::from_u64(out_sum);

    // Bind the public `withdraw_amount` into the statement.
    Bn254Fr::assert_equal_u64(&withdraw_fr, withdraw_amount);

    // Bind the transparent withdraw destination into the statement.
    // We bind as two 16-byte chunks to avoid requiring the full 32 bytes be < BN254 modulus.
    let mut withdraw_to_hi = Bn254Fr::new();
    let mut withdraw_to_lo = Bn254Fr::new();
    withdraw_to_hi.set_bytes_big(&withdraw_to[..16]);
    withdraw_to_lo.set_bytes_big(&withdraw_to[16..]);
    Bn254Fr::assert_equal_bytes_be(&withdraw_to_hi, &withdraw_to[..16]);
    Bn254Fr::assert_equal_bytes_be(&withdraw_to_lo, &withdraw_to[16..]);

    let mut rhs_fr = Bn254Fr::new();
    addmod_checked(&mut rhs_fr, &withdraw_fr, &out_sum_fr);
    Bn254Fr::assert_equal(&sum_in_fr, &rhs_fr);

    // Enforce:
    // - No zero-value notes (inputs and outputs)
    // - Output rho uniqueness vs input rhos
    // - Output rhos pairwise distinct (when n_out == 2)
    //
    // via a single inverse witness `inv_enforce`:
    //   enforce_prod = Π(v_in) * Π(v_out) * Π(rho_out - rho_in) * (rho_out0 - rho_out1 if 2 outs)
    //   enforce_prod * inv_enforce == 1
    //
    // This avoids per-value/per-delta branching and keeps the constraint system fixed.
    let mut delta_fr = Bn254Fr::new();
    for rho_out_fr in &out_rhos_fr {
        for rho_in_fr in &in_rhos_fr {
            submod_checked(&mut delta_fr, rho_out_fr, rho_in_fr);
            enforce_prod.mulmod_checked(&delta_fr);
        }
    }
    if n_out == 2 {
        submod_checked(&mut delta_fr, &out_rhos_fr[0], &out_rhos_fr[1]);
        enforce_prod.mulmod_checked(&delta_fr);
    }

    // inv_enforce [PRIVATE] (field element encoded as 32-byte BE)
    let inv_enforce_bytes = read_hash32(&args, arg_idx);
    arg_idx += 1;
    let inv_enforce_fr = bn254fr_from_hash32_be(&inv_enforce_bytes);
    enforce_prod.mulmod_checked(&inv_enforce_fr);
    Bn254Fr::assert_equal_u64(&enforce_prod, 1);

    // --- Blacklist checks (bucketed non-membership + Merkle membership) ---
    //
    // Layout appended after inv_enforce:
    //   blacklist_root                  [PUBLIC]
    //   For each checked id:
    //     bucket_entries[BL_BUCKET_SIZE] [PRIVATE]
    //     bucket_inv                    [PRIVATE]
    //     bucket_siblings[BL_DEPTH]     [PRIVATE]
    let blacklist_root = read_hash32(&args, arg_idx);
    arg_idx += 1;

    // Sender (current owner) must not be blacklisted.
    assert_not_blacklisted_bucket_from_args(&h, &sender_id, &blacklist_root, &args, &mut arg_idx);

    // Transfers have a "pay recipient" output; withdraws only have change-to-self outputs, already enforced above.
    if withdraw_amount == 0 {
        assert_not_blacklisted_bucket_from_args(
            &h,
            &outs[0].rcp,
            &blacklist_root,
            &args,
            &mut arg_idx,
        );
    }

    // --- Level B: Viewer Attestations ---
    let base_after_outs = arg_idx;
    if base_after_outs != expected_base as usize {
        hard_fail(84);
    }

    if argc == expected_base {
        return;
    }

    let n_viewers: usize = {
        let v = read_u32(&args, base_after_outs, 91) as usize;
        if v > MAX_VIEWERS {
            hard_fail(91);
        }
        v
    };

    let extra_per_viewer = 1 + 1 + 2 * n_out;
    let expected_argc_b = expected_base + 1u32 + (n_viewers as u32) * (extra_per_viewer as u32);
    if argc != expected_argc_b {
        hard_fail(92);
    }

    let mut cm_ins: [Hash32; MAX_INS] = [[0u8; 32]; MAX_INS];
    for i in 0..n_in {
        let inp = &in_plains[i];
        let cm_fr = note_commitment_fr(
            &h,
            &domain,
            inp.v,
            &inp.rho,
            &recipient_owner,
            &inp.sender_id_in,
        );
        cm_ins[i] = bn254fr_to_hash32(&cm_fr);
    }

    let mut out_pts: [[u8; NOTE_PLAIN_LEN]; MAX_OUTS] = [[0u8; NOTE_PLAIN_LEN]; MAX_OUTS];
    for j in 0..n_out {
        encode_note_plain(
            &domain,
            outs[j].v,
            &outs[j].rho,
            &outs[j].rcp,
            &sender_id,
            &cm_ins,
            &mut out_pts[j],
        );
    }

    let mut out_cm_fr: [[Bn254Fr; 32]; MAX_OUTS] =
        std::array::from_fn(|_| std::array::from_fn(|_| Bn254Fr::new()));
    for j in 0..n_out {
        out_cm_fr[j] = hash32_to_fr_bytes_constrained(&outs[j].cm);
    }

    let mut v_idx = base_after_outs + 1; // start right after n_viewers
    for _vi in 0..n_viewers {
        // PUBLIC: commitment/hash of the viewer key material.
        let fvk_commit_pub = read_hash32(&args, v_idx);
        v_idx += 1;

        // PRIVATE: viewer key material (witness). Verifier must not need the real value.
        let fvk_priv = read_hash32(&args, v_idx);
        v_idx += 1;

        let fvk_priv_fr = hash32_to_fr_bytes_range_checked(&fvk_priv);
        let fvk_c_fr = fvk_commit_fr(&h, &fvk_priv_fr);
        assert_fr_eq_hash32(&fvk_c_fr, &fvk_commit_pub);

        for j in 0..n_out {
            let cm_fr = &out_cm_fr[j];
            let k = view_kdf(&h, &fvk_priv_fr, cm_fr);
            let pt_fr = bytes_note_plain_to_fr_bytes(&out_pts[j]);
            let mut ct_fr: [Bn254Fr; NOTE_PLAIN_LEN] = std::array::from_fn(|_| Bn254Fr::new());
            stream_xor_encrypt_note_plain(&h, &k, &pt_fr, &mut ct_fr);

            let ct_h_fr = ct_hash_fr(&h, &ct_fr);
            let ct_hash_arg = read_hash32(&args, v_idx);
            v_idx += 1;
            assert_fr_eq_hash32(&ct_h_fr, &ct_hash_arg);

            let ct_hash_bytes = fr_to_bytes_be_bits(&ct_h_fr);
            let macv_fr = view_mac_fr(&h, &k, cm_fr, &ct_hash_bytes);

            let mac_arg = read_hash32(&args, v_idx);
            v_idx += 1;
            assert_fr_eq_hash32(&macv_fr, &mac_arg);
        }
    }
}
