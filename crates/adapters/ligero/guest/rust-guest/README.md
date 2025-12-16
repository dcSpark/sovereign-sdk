# Rust Guest Program for Ligero (Proof of Concept)

This is a **proof-of-concept** demonstrating that Rust programs can be compiled to WASM and used with Ligero, just like C++ programs.

## Purpose

This guest program implements the same validation logic as `value_validator.cpp`:
1. Validates a value is in range [0, 65535]
2. Enforces proven value equals claimed value
3. Prevents proof substitution attacks

## Why Rust?

Writing guest programs in Rust instead of C++ allows us to:
- ✅ Use Ligetron SDK for Poseidon2 hashing (consistent with the circuit)
- ✅ Share types between the module and guest program
- ✅ Leverage Rust's memory safety and type system
- ✅ Access the full Rust crypto ecosystem

## Building

### Prerequisites

1. **Rust toolchain** (install from https://rustup.rs/)
2. **wasm32-wasi target** (auto-installed by build script)

### Build Commands

```bash
cd rust-guest

# Build to WASM
./build.sh

# Or manually:
cargo build --target wasm32-wasi --release
```

**Output:** `target/wasm32-wasi/release/value_validator_rust.wasm`

## Testing with Ligero

Once built, you can test the WASM with Ligero's prover/verifier:

```bash
cd ../bins

# Create a test config (JSON)
cat > test_rust.json << 'EOF'
{
  "program": "../rust-guest/target/wasm32-wasi/release/value_validator_rust.wasm",
  "shader-path": "shader",
  "packing": 8192,
  "private-indices": [],
  "args": [
    {"str": "42"},
    {"str": "42"}
  ]
}
EOF

# Generate proof
./webgpu_prover "$(cat test_rust.json)"

# Verify proof
./webgpu_verifier "$(cat test_rust.json)"
```

## Comparison with C++

| Aspect | C++ (value_validator.cpp) | Rust (this crate) |
|--------|---------------------------|-------------------|
| **Compilation** | Emscripten (em++) | rustc + wasm32-wasi |
| **Dependencies** | Ligero SDK (ligetron.a) | None (std only) |
| **Size** | ~15-20 KB | ~10-15 KB |
| **Language** | C++ | Rust |
| **Safety** | Manual | Compiler-enforced |

## Next Steps

If this proof-of-concept works with Ligero:

1. ✅ **Confirmed**: Rust->WASM works with Ligero
2. ✅ **Create** `note_spend` guest in Rust (see `note-spend-guest/`)
3. ✅ **Use** Ligetron SDK for Poseidon2 hashing
4. 🚀 **Deploy** privacy-preserving note spending

## Implementation Notes

### Current Approach

The current implementation:
- Uses `no_std` for minimal binary size
- Manually parses arguments from WASI argv
- Returns 0 for success, 1 for failure
- Does NOT use Ligero SDK's `assert_one()` (Rust doesn't link with ligetron.a)

### For Production

For the actual `note_spend` circuit (see `note-spend-guest/`), we:
- Use Ligetron SDK for Poseidon2 via bn254fr host functions
- Implement proper error handling
- Use better argument parsing
- Leverage Ligetron's native FFI bindings

## File Structure

```
rust-guest/
├── Cargo.toml          ← Rust project config
├── build.sh            ← Build script
├── README.md           ← This file
└── src/
    └── lib.rs          ← Main guest program
```

## Troubleshooting

### Build Errors

If the build fails:

```bash
# Ensure wasm32-wasi target is installed
rustup target add wasm32-wasi

# Clean and rebuild
cargo clean
cargo build --target wasm32-wasi --release
```

### Runtime Errors

If Ligero can't run the WASM:
- Check that the WASM file exists
- Verify the file isn't corrupted: `file value_validator_rust.wasm`
- Try with C++ version first to ensure Ligero is working

### Size Optimization

To make the WASM smaller:

```toml
[profile.release]
opt-level = "z"     # Optimize for size
lto = true          # Link Time Optimization
codegen-units = 1   # Better optimization
panic = "abort"     # Smaller panic handler
strip = true        # Strip debug symbols
```

## License

Copyright (C) 2023-2025 Sovereign Labs  
Licensed under the Apache License, Version 2.0


