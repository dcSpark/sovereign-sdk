/*
 * REFERENCE ONLY — NOT USED BY THIS REPO'S BUILD.
 *
 * This is a copied snapshot of the Ligero/Ligetron `note_spend_guest` v2 guest program source
 * (TRANSFER/WITHDRAW verifier). The source of truth for the executable guest remains the Ligero
 * repo artifact `note_spend_guest.wasm`.
 *
 * Kept here so the Sovereign-side adapter/tests can track the expected argument layout and
 * hashing/tag conventions.
 */

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
 *   pos_bits_i[k]      — depth × 32 bytes [PRIVATE; each is 0/1]
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
 * Expected argc (no viewers) =
 *   1 + 6 + n_in*(4 + 2*depth) + 3 + 5*n_out + 1
 * (argc includes argv[0]).
 *
 * SECURITY NOTES:
 *   1) All validation paths inject UNSAT constraints before exit (hard_fail)
 *   2) Balance check uses field-level constraint, not runtime boolean comparison
 *   3) Position bits are constrained to be boolean (0 or 1)
 *   4) Merkle path uses field-level MUX to avoid witness-dependent constraints
 *
 * Hashing uses Ligetron's Poseidon2 via bn254fr host functions (Ligero-compatible).
 *
 * Viewer plaintexts (Level B) are extended to include an attested sender_id:
 *   [ domain | value | rho | recipient | sender_id ]
 *
 */

// =============================================================================
// Ligetron SDK requires std - the heavy Poseidon2 computation is done via
// host functions anyway, so std overhead is minimal.
// =============================================================================

// Ligetron SDK imports
use ligetron::api::{get_args, ArgHolder};
use ligetron::bn254fr::{Bn254Fr, addmod_checked, submod_checked};
use ligetron::poseidon2::poseidon2_hash_bytes;

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
// FIELD-LEVEL SELECT FOR OBLIVIOUS MERKLE PATH
// Uses arithmetic selection with private bits (no witness-dependent branching).
// ============================================================================

/// Enforce that a field element is a boolean (0 or 1).
/// Constraint: cond * (cond - 1) == 0
///
/// This is critical for soundness: if position bits are not constrained
/// to be boolean, a malicious prover could use other field values to
/// manipulate the Merkle path computation.
#[inline(always)]
fn assert_bit(cond: &Bn254Fr, one: &Bn254Fr) {
    // t = cond - 1
    let mut t = Bn254Fr::new();
    submod_checked(&mut t, cond, &one);
    // t = cond * (cond - 1)
    t.mulmod_checked(cond);
    // Assert t == 0 (only true if cond is 0 or 1)
    Bn254Fr::assert_equal_u64(&t, 0);
}

/// Read a position bit from a 32-byte hex argument.
/// The bit should be passed as 0x000...0000 (for 0) or 0x000...0001 (for 1).
/// Returns a Bn254Fr that is either 0 or 1.
#[inline(always)]
fn read_position_bit(args: &ArgHolder, index: usize) -> Bn254Fr {
    let b = read_hash32(args, index);
    bn254fr_from_hash32_be(&b)
}

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
            eprintln!("read_hash32: idx={} len={} (unexpected)", index, bytes.len());
            eprintln!("read_hash32: first_bytes={:02x?}", &bytes[..bytes.len().min(16)]);
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
const CT_HASH_BUF_LEN: usize = 10 + 144; // "CT_HASH_V1" + ct = 154
const VIEW_MAC_BUF_LEN: usize = 11 + 32 + 32 + 32; // "VIEW_MAC_V1" + k + cm + ct_hash = 107

/// Merkle tree node hash: H("MT_NODE_V1" || lvl || left || right)
/// Fixed 75-byte preimage.
fn mt_combine(h: &Poseidon2Core, level: u8, left: &Hash32, right: &Hash32) -> (Bn254Fr, Hash32) {
    let mut buf = [0u8; MT_NODE_BUF_LEN];
    buf[..10].copy_from_slice(b"MT_NODE_V1");
    buf[10] = level;
    buf[11..43].copy_from_slice(left);
    buf[43..75].copy_from_slice(right);
    let fr = h.hash_padded_fr(&buf);
    let bytes = bn254fr_to_hash32(&fr);
    (fr, bytes)
}

/// Note commitment: H("NOTE_V2" || domain || value_16 || rho || recipient || sender_id)
/// Fixed 151-byte preimage. Value is u64 zero-extended to 16 bytes.
///
/// `sender_id` is the attested sender identity bound into the commitment (not just viewer plaintext).
fn note_commitment(
    h: &Poseidon2Core,
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> (Bn254Fr, Hash32) {
    let mut buf = [0u8; NOTE_CM_BUF_LEN];
    buf[..7].copy_from_slice(b"NOTE_V2");
    buf[7..39].copy_from_slice(domain);
    // Encode value as 16-byte LE (zero-extended from u64)
    buf[39..47].copy_from_slice(&value.to_le_bytes());
    // buf[47..55] already zero from initialization (zero-extension)
    buf[55..87].copy_from_slice(rho);
    buf[87..119].copy_from_slice(recipient);
    buf[119..151].copy_from_slice(sender_id);
    let fr = h.hash_padded_fr(&buf);
    let bytes = bn254fr_to_hash32(&fr);
    (fr, bytes)
}

/// Nullifier: H("PRF_NF_V1" || domain || nf_key || rho)
/// Fixed 105-byte preimage.
fn nullifier(h: &Poseidon2Core, domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> (Bn254Fr, Hash32) {
    let mut buf = [0u8; PRF_NF_BUF_LEN];
    buf[..9].copy_from_slice(b"PRF_NF_V1");
    buf[9..41].copy_from_slice(domain);
    buf[41..73].copy_from_slice(nf_key);
    buf[73..105].copy_from_slice(rho);
    let fr = h.hash_padded_fr(&buf);
    let bytes = bn254fr_to_hash32(&fr);
    (fr, bytes)
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
fn recipient_from_pk(h: &Poseidon2Core, domain: &Hash32, pk_spend: &Hash32, pk_ivk: &Hash32) -> Hash32 {
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

// ============================================================================
// OBLIVIOUS MERKLE PATH: Avoid secret-dependent branching.
// If `pos` is private, `if (pos_bit) { ... }` creates secret branching which
// is expensive in Ligetron. Instead, use constant-time conditional swap.
// ============================================================================

/// Compute Merkle root using FIELD-LEVEL MUX operations.
/// This version works with private position bits and siblings!
///
/// Arguments:
/// - h: Poseidon2 hasher instance
/// - leaf_bytes: The leaf commitment as 32-byte hash
/// - pos_bits: Position bits as field elements (0 or 1 each), one per level
/// - siblings_fr: Sibling hashes as field elements, one per level
/// - depth: Number of levels in the Merkle path
///
/// The position bits determine which side the current node is on at each level:
/// - bit=0: current is left child, sibling is right child
/// - bit=1: current is right child, sibling is left child
fn root_from_path_field_level(
    h: &Poseidon2Core,
    leaf_bytes: &Hash32,
    pos_bits: &[Bn254Fr],
    siblings_fr: &[Bn254Fr],
    depth: usize,
) -> Bn254Fr {
    if depth == 0 {
        hard_fail(77);
    }

    // Initialize current node from leaf
    let mut cur_fr = bn254fr_from_hash32_be(leaf_bytes);

    // Reuse temporaries; this also reduces per-level host overhead.
    let mut left_fr = Bn254Fr::new();
    let mut right_fr = Bn254Fr::new();
    let mut delta = Bn254Fr::new();
    let mut left_bytes = [0u8; 32];
    let mut right_bytes = [0u8; 32];

    let mut lvl = 0usize;
    while lvl < depth {
        let bit = &pos_bits[lvl];
        let sib_fr = &siblings_fr[lvl];

        // 1-mul select:
        // delta = bit * (sib - cur)
        // left  = cur + delta
        // right = sib - delta
        submod_checked(&mut delta, sib_fr, &cur_fr);
        delta.mulmod_checked(bit);
        addmod_checked(&mut left_fr, &cur_fr, &delta);
        submod_checked(&mut right_fr, sib_fr, &delta);

        left_fr.get_bytes_big(&mut left_bytes);
        right_fr.get_bytes_big(&mut right_bytes);

        // Compute hash using byte preimage.
        let (next_fr, _next_bytes) = mt_combine(h, lvl as u8, &left_bytes, &right_bytes);
        cur_fr = next_fr;
        lvl += 1;
    }

    cur_fr
}

// === Level B: Viewer Attestation Functions ===

/// FVK commitment: H("FVK_COMMIT_V1" || fvk)
/// Fixed 45-byte preimage.
fn fvk_commit(h: &Poseidon2Core, fvk: &Hash32) -> (Bn254Fr, Hash32) {
    let mut buf = [0u8; FVK_COMMIT_BUF_LEN];
    buf[..13].copy_from_slice(b"FVK_COMMIT_V1");
    buf[13..45].copy_from_slice(fvk);
    let fr = h.hash_padded_fr(&buf);
    let bytes = bn254fr_to_hash32(&fr);
    (fr, bytes)
}

/// View KDF: H("VIEW_KDF_V1" || fvk || cm)
/// Fixed 75-byte preimage.
fn view_kdf(h: &Poseidon2Core, fvk: &Hash32, cm: &Hash32) -> Hash32 {
    let mut buf = [0u8; VIEW_KDF_BUF_LEN];
    buf[..11].copy_from_slice(b"VIEW_KDF_V1");
    buf[11..43].copy_from_slice(fvk);
    buf[43..75].copy_from_slice(cm);
    h.hash_padded(&buf)
}

/// Stream block: H("VIEW_STREAM_V1" || k || ctr)
/// Fixed 50-byte preimage.
fn stream_block(h: &Poseidon2Core, k: &Hash32, ctr: u32) -> Hash32 {
    let mut buf = [0u8; VIEW_STREAM_BUF_LEN];
    buf[..14].copy_from_slice(b"VIEW_STREAM_V1");
    buf[14..46].copy_from_slice(k);
    buf[46..50].copy_from_slice(&ctr.to_le_bytes());
    h.hash_padded(&buf)
}

/// Stream XOR encrypt for exactly 144 bytes (NOTE_PLAIN_LEN).
/// Optimized: 5 hash calls for 144 bytes (4 full blocks + 16-byte remainder).
fn stream_xor_encrypt_144(h: &Poseidon2Core, k: &Hash32, pt: &[u8; 144], ct_out: &mut [u8; 144]) {
    // Block 0: bytes 0-31
    let ks0 = stream_block(h, k, 0);
    let mut i = 0;
    while i < 32 {
        ct_out[i] = pt[i] ^ ks0[i];
        i += 1;
    }

    // Block 1: bytes 32-63
    let ks1 = stream_block(h, k, 1);
    i = 0;
    while i < 32 {
        ct_out[32 + i] = pt[32 + i] ^ ks1[i];
        i += 1;
    }

    // Block 2: bytes 64-95
    let ks2 = stream_block(h, k, 2);
    i = 0;
    while i < 32 {
        ct_out[64 + i] = pt[64 + i] ^ ks2[i];
        i += 1;
    }

    // Block 3: bytes 96-127
    let ks3 = stream_block(h, k, 3);
    i = 0;
    while i < 32 {
        ct_out[96 + i] = pt[96 + i] ^ ks3[i];
        i += 1;
    }

    // Block 4: bytes 128-143 (16-byte remainder)
    let ks4 = stream_block(h, k, 4);
    i = 0;
    while i < 16 {
        ct_out[128 + i] = pt[128 + i] ^ ks4[i];
        i += 1;
    }
}

/// Ciphertext hash: H("CT_HASH_V1" || ct)
/// Fixed 154-byte preimage for 144-byte ciphertext.
fn ct_hash(h: &Poseidon2Core, ct: &[u8; 144]) -> (Bn254Fr, Hash32) {
    let mut buf = [0u8; CT_HASH_BUF_LEN];
    buf[..10].copy_from_slice(b"CT_HASH_V1");
    buf[10..154].copy_from_slice(ct);
    let fr = h.hash_padded_fr(&buf);
    let bytes = bn254fr_to_hash32(&fr);
    (fr, bytes)
}

/// View MAC: H("VIEW_MAC_V1" || k || cm || ct_hash)
/// Fixed 107-byte preimage.
fn view_mac(h: &Poseidon2Core, k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> (Bn254Fr, Hash32) {
    let mut buf = [0u8; VIEW_MAC_BUF_LEN];
    buf[..11].copy_from_slice(b"VIEW_MAC_V1");
    buf[11..43].copy_from_slice(k);
    buf[43..75].copy_from_slice(cm);
    buf[75..107].copy_from_slice(ct_h);
    let fr = h.hash_padded_fr(&buf);
    let bytes = bn254fr_to_hash32(&fr);
    (fr, bytes)
}

/// Encode note plaintext for viewer encryption.
/// [ domain(32) | value_le_16 | rho(32) | recipient(32) | sender_id(32) ] => 144 bytes
/// Value is u64 zero-extended to 16 bytes.
fn encode_note_plain(domain: &Hash32, value: u64, rho: &Hash32, recipient: &Hash32, sender_id: &Hash32, out: &mut [u8; 144]) {
    out[0..32].copy_from_slice(domain);
    // Encode value as 16-byte LE (u64 zero-extended to 16 bytes)
    out[32..40].copy_from_slice(&value.to_le_bytes());
    // Explicitly zero the high 8 bytes for self-contained correctness
    // (don't rely on caller to pre-zero the buffer)
    out[40..48].copy_from_slice(&[0u8; 8]);
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
}

/// Maximum Merkle tree depth supported by the circuit.
/// Must be ≤ 63 to ensure the bound check `pos >= (1u64 << depth)` is safe
/// (shifting by 64 would overflow a u64).
const MAX_DEPTH: usize = 63;
const MAX_INS: usize = 4;
const MAX_OUTS: usize = 2;
const MAX_VIEWERS: usize = 8;
const NOTE_PLAIN_LEN: usize = 144; // 32 + 16 + 32 + 32 + 32 (domain + value + rho + recipient + sender_id)

fn main() {
    let args = get_args();
    let argc = args.len() as u32;

    // Create single hasher instance, reuse for all hashes.
    let h = Poseidon2Core::new();
    // A real 1 value for boolean constraints.
    // We set the witness value and also bind it as a constant to avoid relying on backend semantics.
    let one = Bn254Fr::from_u32(1);
    Bn254Fr::assert_equal_u64(&one, 1);

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

        // pos_bits_i[k] [PRIVATE]
        let mut pos_bits: Vec<Bn254Fr> = Vec::with_capacity(depth);
        for _lvl in 0..depth {
            let bit = read_position_bit(&args, arg_idx);
            assert_bit(&bit, &one);
            pos_bits.push(bit);
            arg_idx += 1;
        }

        // siblings_i[k] [PRIVATE] (field elements for MUX)
        let mut siblings_fr: Vec<Bn254Fr> = Vec::with_capacity(depth);
        for _lvl in 0..depth {
            let sibling = read_hash32(&args, arg_idx);
            siblings_fr.push(bn254fr_from_hash32_be(&sibling));
            arg_idx += 1;
        }

        // nullifier_i [PUBLIC]
        let nullifier_arg_i = read_hash32(&args, arg_idx);
        arg_idx += 1;
        nullifier_args.push(nullifier_arg_i);

        // Verify Merkle membership for this input.
        let (_cm_i_fr, cm_i_bytes) =
            note_commitment(&h, &domain, v_i, &rho_i, &recipient_owner, &sender_id_in_i);
        let anchor_i_fr =
            root_from_path_field_level(&h, &cm_i_bytes, &pos_bits[..depth], &siblings_fr[..depth], depth);
        assert_fr_eq_hash32(&anchor_i_fr, &anchor_arg);

        // Verify nullifier for this input.
        let (nf_i_fr, _nf_i_bytes) = nullifier(&h, &domain, &nf_key, &rho_i);
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
    //   1 + 6 + n_in*(4 + 2*depth) + 3 + 5*n_out + 1(inv_enforce)
    let per_in = 4u32 + 2u32 * depth_u32;
    let expected_base = 1u32 + 6u32 + n_in_u32 * per_in + 3u32 + 5u32 * n_out_u32 + 1u32;
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

        let (cm_cmp_fr, _cm_cmp_bytes) =
            note_commitment(&h, &domain, vj, &rho_j, &rcp_j, &sender_id);
        assert_fr_eq_hash32(&cm_cmp_fr, &cm_arg);

        outs[j] = OutPlain {
            v: vj,
            rho: rho_j,
            rcp: rcp_j,
            cm: cm_arg,
        };
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

    // --- Level B: Viewer Attestations ---
    let base_after_outs = arg_idx;
    if base_after_outs != expected_base as usize {
        hard_fail(84);
    }

    if argc == expected_base {
        return;
    }

    // Disallow viewers when n_out == 0.
    if n_out == 0 {
        hard_fail(91);
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

    let mut out_pts: [[u8; NOTE_PLAIN_LEN]; MAX_OUTS] = [[0u8; NOTE_PLAIN_LEN]; MAX_OUTS];
    for j in 0..n_out {
        encode_note_plain(
            &domain,
            outs[j].v,
            &outs[j].rho,
            &outs[j].rcp,
            &sender_id,
            &mut out_pts[j],
        );
    }

    let mut ct_buf = [0u8; NOTE_PLAIN_LEN];

    let mut v_idx = base_after_outs + 1; // start right after n_viewers
    for _vi in 0..n_viewers {
        let fvk_commit_arg = read_hash32(&args, v_idx);
        v_idx += 1;

        let fvk = read_hash32(&args, v_idx);
        v_idx += 1;

        let (fvk_c_fr, _fvk_c_bytes) = fvk_commit(&h, &fvk);
        assert_fr_eq_hash32(&fvk_c_fr, &fvk_commit_arg);

        for j in 0..n_out {
            let outp = &outs[j];
            let k = view_kdf(&h, &fvk, &outp.cm);
            stream_xor_encrypt_144(&h, &k, &out_pts[j], &mut ct_buf);

            let (ct_h_fr, ct_h_bytes) = ct_hash(&h, &ct_buf);
            let (macv_fr, _macv_bytes) = view_mac(&h, &k, &outp.cm, &ct_h_bytes);

            let ct_hash_arg = read_hash32(&args, v_idx);
            v_idx += 1;
            assert_fr_eq_hash32(&ct_h_fr, &ct_hash_arg);

            let mac_arg = read_hash32(&args, v_idx);
            v_idx += 1;
            assert_fr_eq_hash32(&macv_fr, &mac_arg);
        }
    }
}

