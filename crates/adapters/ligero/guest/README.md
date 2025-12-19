# Ligero Guest Programs

This directory contains Ligero guest programs (WASM modules) that run inside zero-knowledge proofs. Each guest program implements specific validation logic that can be proven and verified cryptographically.

## Available Guest Programs

### 1. `rust-guest/` - Value Validator (PoC)

**Purpose**: Proof-of-concept demonstrating Rust-to-WASM compilation for Ligero.

A simple validator that checks:
- Value is within range `[0, 65535]` (u16)
- Proven value matches claimed value

**Technology**: Pure Rust `no_std`, minimal dependencies

See: [`rust-guest/README.md`](./rust-guest/README.md)

### 2. `note-spend-guest/` - Note Spend Verifier (Production)

**Purpose**: Production-grade shielded pool spend verification for the Midnight Privacy module.

Verifies:
- Merkle root recomputation from note commitment and path
- PRF-based nullifier derivation
- Solvency constraints (withdraw ≤ note value)
- Public value binding (anchor, nullifier, withdraw amount)

**Technology**: Rust `no_std` with Poseidon2 cryptography (p3-poseidon2, p3-goldilocks)

See: [`note-spend-guest/README.md`](./note-spend-guest/README.md)

### 3. Legacy C++ Programs

The `value_validator.cpp` and related C++/WAT programs are legacy implementations. They are kept for reference and backward compatibility.

## Quick Start

### Building Rust Guest Programs

Each Rust guest has its own build script:

```bash
# Build value validator (PoC)
cd rust-guest
./build.sh

# Build note spend verifier (production)
cd note-spend-guest
./build.sh
```

Compiled WASM modules are placed in `bins/programs/`.

### Prerequisites

- Rust toolchain (edition 2021)
- `wasm32-wasip1` target: `rustup target add wasm32-wasip1`

## Integration with Sovereign SDK

### Using a Guest Program

```rust
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment, LigeroVerifier};
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};

// 1. Load guest program
let program_path = "path/to/guest.wasm";
let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);

// 2. Add arguments
host.add_string_arg(&arg1);
host.add_i64_arg(arg2);
// ... etc

// 3. Set public output
host.set_public_output(&output_data)?;

// 4. Generate proof
let proof = host.run(true)?;

// 5. Verify proof
let code_commitment = host.code_commitment();
let verified_output = LigeroVerifier::verify(&proof, &code_commitment)?;
```

### Configuration

Guest programs are configured via JSON:

```json
{
  "program": "path/to/guest.wasm",
  "private-indices": [5, 8, 9, 10],
  "packing": 8192
}
```

Private indices specify which arguments should be hidden from the verifier.

## Guest Program Structure

All guest programs follow this pattern:

```rust
#![no_std]

// 1. WASI imports
extern "C" {
    fn assert_one(x: i32);
    fn args_get(argv_ptrs: *mut *mut u8, argv_buf: *mut u8) -> u32;
    fn proc_exit(code: u32) -> !;
}

// 2. Entry point
#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    // Parse arguments
    // Compute values
    // Assert constraints
    assert_one(condition as i32);
    proc_exit(0)
}
```

### Key Principles

1. **No Standard Library**: Use `#![no_std]` for minimal WASM size
2. **WASI Command Module**: Entry point is `_start()`
3. **Cryptographic Assertions**: Use `assert_one()` for constraints
4. **Determinism**: Must produce same output for same input
5. **Privacy**: Mark sensitive arguments as private in config

## Development Guidelines

### Writing a New Guest Program

1. **Create directory structure**:
   ```
   my-guest/
   ├── Cargo.toml
   ├── build.sh
   ├── README.md
   └── src/
       └── lib.rs
   ```

2. **Configure Cargo.toml**:
   ```toml
   [package]
   name = "my-guest"
   edition = "2021"
   
   [lib]
   crate-type = ["cdylib"]
   
   [dependencies]
   # no_std allocator if needed
   dlmalloc = { version = "0.2", default-features = false }
   ```

3. **Implement verification logic** in `src/lib.rs`

4. **Build and test**:
   ```bash
   ./build.sh
   # Test integration with Ligero adapter
   ```

### Best Practices

- ✅ Use `#![no_std]` to minimize WASM size
- ✅ Parse arguments carefully (validate all inputs)
- ✅ Use constant-time comparisons for secrets
- ✅ Document the ABI in comments and README
- ✅ Match on-chain cryptography exactly (same hashes, etc.)
- ✅ Include comprehensive tests
- ❌ Don't use panics (exit with error code instead)
- ❌ Don't depend on external state or I/O
- ❌ Don't use floating-point operations (not deterministic)

## Testing

Integration tests are located in the modules that use these guest programs:

```bash
# Test note-spend-guest with midnight-privacy module
cd crates/module-system/module-implementations/midnight-privacy
cargo test --features native ligero_proof_test -- --nocapture
```

## Debugging

### Generate WAT Files

To inspect the WebAssembly text format:

```bash
./build.sh --wat
# Generates bins/programs/guest_name.wat
```

### Check WASM Size

```bash
ls -lh bins/programs/guest_name.wasm
```

Smaller WASM = faster proving. Target: < 100 KB for most guests.

### Common Issues

**"error: linking with `rust-lld` failed"**
- Ensure `wasm32-wasip1` target is installed
- Check RUSTFLAGS are set correctly

**"undefined symbol: assert_one"**
- This is expected; the host provides `assert_one` at runtime
- Make sure you're using `#[link(wasm_import_module = "env")]`

**"proof verification failed"**
- Check that arguments match expected types and order
- Verify private-indices configuration
- Ensure cryptography matches on-chain implementation

## Architecture

```
guest/
├── README.md              # This file
├── bins/                  # Output directory
│   └── programs/
│       ├── value_validator_rust.wasm
│       └── note_spend_guest.wasm
├── rust-guest/            # PoC: value validator
│   ├── Cargo.toml
│   ├── build.sh
│   └── src/lib.rs
├── note-spend-guest/      # Production: note spend verifier
│   ├── Cargo.toml
│   ├── build.sh
│   └── src/lib.rs
└── value_validator.cpp    # Legacy C++ program
```

## Performance Characteristics

| Program | Size | Proving Time* | Use Case |
|---------|------|---------------|----------|
| value-validator | ~500 B | < 1s | Simple range check |
| note-spend-guest | 50 KB | ~30s** | Shielded pool spend |

\* Approximate times on WebGPU-capable hardware  
\*\* Actual proving time depends on Merkle tree depth and hardware

## References

- [Ligero Paper](https://eprint.iacr.org/2022/1608)
- [WebAssembly Specification](https://webassembly.github.io/spec/)
- [WASI Documentation](https://github.com/WebAssembly/WASI)
- [Rust Embedded Book](https://docs.rust-embedded.org/book/)
- [no_std Guide](https://docs.rust-embedded.org/book/intro/no-std.html)

## Contributing

When adding a new guest program:

1. Create a new directory with clear naming
2. Include comprehensive README.md
3. Add build script and usage examples
4. Write integration tests in the appropriate module
5. Update this README with a new entry

## License

Copyright (C) 2023-2025 Sovereign Labs  
Licensed under the Apache License, Version 2.0
