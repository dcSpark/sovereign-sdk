# Midnight Privacy Integration Tests

This directory contains integration tests for the `midnight-privacy` module.

## Test Files

### `tests.rs`
Basic serialization and configuration tests for the module's data structures.

### `ligero_proof_test.rs` ✨ NEW
Comprehensive integration tests demonstrating **REAL** Ligero proof generation and verification.

## Ligero Proof Tests

The `ligero_proof_test.rs` file contains tests that demonstrate:

1. ✅ **Real proof generation** - Uses `webgpu_prover` with WebGPU acceleration
2. ✅ **Real proof verification** - Uses `webgpu_verifier` with actual cryptographic verification
3. ✅ **Code commitment verification** - Ensures proofs match expected programs
4. ✅ **Security tests** - Validates that proof substitution attacks are prevented
5. ✅ **Boundary value testing** - Tests edge cases (0, 65535)

### Test Coverage

| Test | Description |
|------|-------------|
| `test_ligero_proof_generation_and_verification` | End-to-end proof generation and verification for value 42 |
| `test_ligero_proof_with_different_values` | Tests multiple valid values (0, 1, 100, 1000, 65535) |
| `test_ligero_proof_code_commitment_mismatch` | Verifies that wrong code commitments are rejected |
| `test_ligero_code_commitment_encoding` | Tests codec commitment encoding/decoding |
| `test_ligero_code_commitment_invalid_length` | Tests length validation for code commitments |
| `test_ligero_proof_value_mismatch_detected` | Ensures proven_value == claimed_value is enforced |
| `test_ligero_proof_with_boundary_values` | Tests minimum (0) and maximum (65535) values |

## Requirements

To run the Ligero proof tests, you need:

### 1. Hardware & Software
- WebGPU-capable hardware (GPU)
- Modern browser or runtime with WebGPU support

### 2. Ligero Binaries
- `webgpu_prover` - For proof generation
- `webgpu_verifier` - For proof verification
- Shader files - For GPU computation

### 3. Guest Program
- `value_validator_rust.wasm` - The WASM program that enforces constraints
  - Located at: `<ligero-prover>/utils/circuits/bins/value_validator_rust.wasm`

### 4. Automatic Configuration ✨

**NEW:** Tests now use `setup_ligero_env()` which automatically:
- Discovers paths to Ligero binaries based on project structure
- Sets environment variables for verification
- Validates that required files exist

**You don't need to manually set environment variables!**

### Optional: Manual Environment Override

If you need to override the default paths, you can set these environment variables:

```bash
# Optional manual overrides
export LIGERO_VERIFIER_BIN="path/to/webgpu_verifier"
export LIGERO_PROGRAM_PATH="path/to/value_validator_rust.wasm"
export LIGERO_SHADER_PATH="path/to/shader"
export LIGERO_PACKING=8192  # optional, defaults to 8192
```

## Running the Tests

### Run all integration tests:
```bash
cargo test --test integration --features native
```

### Run only Ligero proof tests:
```bash
cargo test --test integration ligero_proof --features native
```

### Run a specific test:
```bash
cargo test --test integration test_ligero_proof_generation_and_verification --features native -- --nocapture
```

### Run with verbose output:
```bash
cargo test --test integration ligero_proof --features native -- --nocapture --test-threads=1
```

## What Makes These Tests "Real"?

Unlike many zkVM tests that use simulation or skip verification, these tests:

1. **Generate actual cryptographic proofs** using `webgpu_prover`
2. **Verify proofs cryptographically** using `webgpu_verifier`
3. **Check code commitments** to prevent proof substitution attacks
4. **Enforce guest program constraints** in zero-knowledge
5. **Use real GPU acceleration** via WebGPU

**No shortcuts. No simulation. Real zero-knowledge proofs.**

## Guest Program: value_validator_rust.wasm

The tests use a C++ guest program (`value_validator.cpp`) that enforces:

```cpp
// Proven value must be in range [0, 65535]
assert_one(proven_value >= 0);
assert_one(proven_value <= 65535);

// Proven value must match claimed value (prevents substitution)
assert_one(proven_value == claimed_value);
```

This demonstrates how Ligero can enforce arbitrary constraints in zero-knowledge.

## Troubleshooting

### Test skips with "value_validator_rust.wasm not found"
- Build the guest program in the Ligero repo (or set `LIGERO_PROGRAM_PATH` to an explicit wasm path).

### "Failed to execute webgpu_prover"
- Ensure the prover binary is in your PATH or use absolute paths
- Check that you have WebGPU-capable hardware

### "Ligero verifier configuration error"
- Set all required environment variables (see above)
- Use absolute paths or run from repository root

### Proof generation/verification fails
- Check GPU availability
- Ensure WebGPU drivers are installed
- Try running individual tests with `--test-threads=1`

## Example Output

```
Code commitment (method_id): a1b2c3d4...
Generating REAL proof with WebGPU...
✓ REAL proof generated successfully! Size: 2847563 bytes
Verifying proof with REAL verifier...
✓ REAL proof verified successfully!
```

## Security Notes

These tests demonstrate important security properties:

1. **Code Commitment Binding**: Proofs are bound to specific WASM programs
2. **Value Binding**: Proven values must match claimed values
3. **Range Constraints**: Values must be in valid ranges
4. **No Simulation Bypass**: Tests use real cryptographic verification

This makes them suitable as examples for production use cases.

