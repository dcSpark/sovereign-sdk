# Note Spend Guest - Zero-Knowledge Proof Circuit for Midnight Privacy Pool

This is a `no_std`, WASI-command Rust guest program for the Ligero zero-knowledge proof system. It verifies a **single-input note spend** for the Midnight Privacy shielded pool.

## Overview

The guest program implements the core cryptographic verification logic for spending a private note:

1. **Merkle Root Recomputation**: Verifies that a note commitment is part of the commitment tree by recomputing the root from a provided Merkle path.
2. **Nullifier Computation**: Derives the nullifier from the note's secret key and randomness using a PRF (Pseudo-Random Function).
3. **Public Value Binding**: Ensures integrity by asserting:
   - Computed `anchor_root` == provided `anchor_root`
   - Computed `nullifier` == provided `nullifier`
   - `withdraw_amount` ≤ note's `value` (solvency check)

## Cryptographic Primitives

### Poseidon2 Hash Function

The program uses **Ligetron's Poseidon2**, a zero-knowledge-friendly hash function optimized for Ligero arithmetic circuits:

- **Field**: BN254 scalar field
- **Width**: 2 (t=2)
- **Implementation**: Ligetron SDK (bn254fr host functions)

The implementation uses Ligetron's native Poseidon2, which is consistent with the Ligero circuit. The on-chain `midnight-privacy` module also uses Ligetron's Poseidon2 to ensure hash compatibility.

### Domain Separation

All hash operations use unique domain tags to prevent cross-domain collisions:

- `MT_NODE_V1`: Merkle tree node hashing
- `NOTE_V1`: Note commitment computation
- `PRF_NF_V1`: Nullifier derivation

## ABI / Input Arguments

The program receives arguments via WASI `args_get` (all as ASCII strings):

| Index | Name | Type | Description | Privacy |
|-------|------|------|-------------|---------|
| 1 | `domain_hex` | 32-byte hex | Domain tag for all hashes | Public |
| 2 | `value_dec` | u128 decimal | Note value | Private* |
| 3 | `rho_hex` | 32-byte hex | Note randomness/nonce | Private* |
| 4 | `recipient_hex` | 32-byte hex | Recipient binding | Private* |
| 5 | `nf_key_hex` | 32-byte hex | Nullifier secret key | **PRIVATE** |
| 6 | `pos_dec` | u64 decimal | Leaf index in tree | Private* |
| 7 | `depth_dec` | u32 decimal | Merkle tree depth | Public |
| 8..8+depth | `siblings_hex[i]` | 32-byte hex each | Merkle path siblings | **PRIVATE** |
| 8+depth | `anchor_hex` | 32-byte hex | Expected Merkle root | Public |
| 9+depth | `nullifier_hex` | 32-byte hex | Expected nullifier | Public |
| 10+depth | `withdraw_amount_dec` | u128 decimal | Withdrawal amount | Public |

\* These values are part of the note opening and should be marked private in the prover configuration.

### Privacy Configuration

In your Ligero prover configuration JSON, mark these indices as private:

```json
{
  "private-indices": [5, 8, 9, 10, ...],
  // Indices 5 (nf_key) and 8..8+depth (siblings) must be private
  // Optionally mark 2,3,4,6 (note opening) as private
}
```

## Building

### Prerequisites

- Rust toolchain (edition 2021)
- `wasm32-wasip1` target

### Build Instructions

```bash
# From this directory
./build.sh

# To also generate a WAT (WebAssembly Text) file for inspection:
./build.sh --wat
```

The compiled WASM module will be placed in:
```
../bins/programs/note_spend_guest.wasm
```

### Manual Build

```bash
rustup target add wasm32-wasip1
RUSTFLAGS="-C link-arg=-s" cargo build --target wasm32-wasip1 --release
```

## Integration

### Example: Running with Ligero Prover

```rust
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment, LigeroVerifier};
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};

// Create host with the note-spend guest program
let program_path = "path/to/note_spend_guest.wasm";
let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);

// Add arguments (example values)
host.add_string_arg(&hex::encode(domain));         // 1: domain
host.add_string_arg(&value.to_string());           // 2: value
host.add_string_arg(&hex::encode(rho));            // 3: rho
host.add_string_arg(&hex::encode(recipient));      // 4: recipient
host.add_string_arg(&hex::encode(nf_key));         // 5: nf_key (PRIVATE)
host.add_string_arg(&pos.to_string());             // 6: pos
host.add_string_arg(&depth.to_string());           // 7: depth

// Add siblings
for sibling in &siblings {
    host.add_string_arg(&hex::encode(sibling));    // 8..8+depth: siblings (PRIVATE)
}

host.add_string_arg(&hex::encode(anchor_root));    // anchor
host.add_string_arg(&hex::encode(nullifier));      // nullifier
host.add_string_arg(&withdraw_amount.to_string()); // withdraw_amount

// Set public output
let public_output = SpendPublic {
    anchor_root,
    nullifier,
    withdraw_amount,
};
host.set_public_output(&public_output)?;

// Generate proof
let proof_data = host.run(true)?;

// Verify proof
let code_commitment = host.code_commitment();
let verified: SpendPublic = LigeroVerifier::verify(&proof_data, &code_commitment)?;
```

## Security Considerations

### Current Implementation

The guest currently binds:
- ✅ `anchor_root`: Ensures note is in the commitment tree
- ✅ `nullifier`: Prevents double-spending
- ✅ `withdraw_amount`: Enforces solvency (amount ≤ note value)

### Recommended Hardening

For production use, also bind:

1. **Chain ID**: Prevent cross-chain replay attacks
2. **Module ID**: Prevent cross-module replay attacks  
3. **Withdraw Address**: Prevent address-substitution attacks

Add these as additional arguments and assert them in the guest:

```rust
// Additional arguments (example)
let chain_id = parse_u64_dec(&tmp[..n])?;
let module_id = parse_hex32(&tmp[..n])?;
let withdraw_to = parse_address(&tmp[..n])?;

// Assert equality with claimed public values
assert_one((chain_id == claimed_chain_id) as i32);
assert_one(eq_bytes(&module_id, &claimed_module_id) as i32);
assert_one(eq_bytes(&withdraw_to, &claimed_withdraw_to) as i32);
```

The module should then verify these values on-chain.

## Testing

See the integration tests in:
```
crates/module-system/module-implementations/midnight-privacy/tests/integration/ligero_proof_test.rs
```

Example test that uses this guest program:

```bash
cd crates/module-system/module-implementations/midnight-privacy
cargo test --features native ligero_proof_test -- --nocapture
```

## Architecture

### File Structure

```
note-spend-guest/
├── Cargo.toml          # Dependencies and build configuration
├── build.sh            # Build script
├── README.md           # This file
└── src/
    └── lib.rs          # Main guest program implementation
```

### Key Functions

- `note_commitment()`: Computes Poseidon2(domain || value || rho || recipient)
- `nullifier()`: Computes Poseidon2(domain || nf_key || rho)
- `mt_combine()`: Combines two Merkle tree nodes with level
- `root_from_path()`: Recomputes root from leaf + path
- `_start()`: WASI entry point, parses arguments and runs verification

## Comparison with PoC

The original `rust-guest` (value validator) is kept as a proof-of-concept. Key differences:

| Feature | value-validator (PoC) | note-spend-guest (Production) |
|---------|----------------------|-------------------------------|
| Purpose | Simple range check | Full shielded spend verification |
| Cryptography | None | Poseidon2, Merkle trees, PRFs |
| Arguments | 2 i32 values | 11+ string arguments |
| Privacy | None | Private witnesses (nf_key, path) |
| Dependencies | None (pure no_std) | p3-poseidon2, p3-goldilocks |
| Size | ~1-2 KB | ~50-100 KB (estimated) |

## Dependencies

- `ligetron`: Ligetron SDK providing Poseidon2 via bn254fr host functions (same implementation as the circuit)

This ensures perfect compatibility with the on-chain hashing, as both the circuit and the `midnight-privacy` module use Ligetron's Poseidon2.

## License

Copyright (C) 2023-2025 Sovereign Labs  
Licensed under the Apache License, Version 2.0

## References

- [Poseidon2 Paper](https://eprint.iacr.org/2023/323)
- [Ligetron SDK](./sdk/rust)
- [Zcash Sapling Protocol](https://github.com/zcash/zips/blob/main/protocol/protocol.pdf)

