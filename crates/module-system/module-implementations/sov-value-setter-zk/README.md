# Value Setter ZK Module

A Sovereign SDK module that allows setting a value in state only after verifying a zero-knowledge proof that the value meets certain constraints.

## Overview

The `value-setter-zk` module demonstrates integration of the Ligero zkVM with Sovereign SDK. Users must provide a valid Ligero proof alongside the value they wish to set. The proof cryptographically demonstrates that the value meets the required constraints without revealing any private information.

## Features

- **ZK Proof Verification**: Uses Ligero zkVM to verify proofs on-chain
- **Value Constraints**: Enforces that values are within the range `[0, 100]`
- **Upgradeable**: Admin can update the method ID (code commitment) to change the guest program
- **Gas Accounting**: Supports optional gas charging for proof verification

## Architecture

### Off-Chain (Proof Generation)

1. User compiles the guest program (`value_validator.wasm`) using Emscripten
2. User runs the proof generator tool with their desired value
3. Tool invokes Ligero prover to generate a proof
4. Tool creates a `LigeroProofPackage` containing:
   - Raw Ligero proof bytes
   - Public output (`ValueProofPublic { value: u32 }`)
5. Package is serialized and ready for submission

### On-Chain (Proof Verification)

1. User submits `SetValueWithProof` transaction with proof and value
2. Module extracts the method ID (code commitment) from state
3. Module uses `LigeroVerifier` to verify the proof
4. Verifier deserializes `LigeroProofPackage` and extracts public output
5. Module checks that `public_output.value == requested_value`
6. If all checks pass, the value is set in state

## Module State

- `value`: The current value (u32)
- `method_id`: Code commitment (32-byte SHA-256 hash) of the guest program
- `admin`: Administrator address who can update the method ID

## Call Messages

### SetValueWithProof

Set a new value with ZK proof verification.

```rust
SetValueWithProof {
    value: u32,
    proof: SafeVec<u8, 1_000_000>, // Serialized LigeroProofPackage
    gas: Option<S::Gas>,
}
```

**Validation:**
1. Proof must verify against the configured method ID
2. Public output must match the requested value
3. Value must be within [0, 100] (enforced by guest program)

### UpdateMethodId

Update the Ligero method ID (admin only).

```rust
UpdateMethodId {
    new_method_id: [u8; 32],
}
```

## Events

- `ValueSetWithProof { value: u32 }`: Emitted when a value is successfully set
- `MethodIdUpdated { new_method_id: [u8; 32] }`: Emitted when the method ID is updated

## Genesis Configuration

```json
{
  "initial_value": 0,
  "method_id": "0x1234...",  // 32-byte hex string
  "admin": "0xabcd..."       // Address of admin
}
```

## End-to-End Workflow

### 1. Build the Guest Program

```bash
cd crates/adapters/ligero/guest
mkdir build && cd build
emcmake cmake ..
emmake make
cp value_validator.wasm ../bins/programs/
```

### 2. Generate a Proof

```bash
cd crates/adapters/ligero
cargo run --example generate_value_proof --features native -- 42
```

This creates:
- `value_proof.bin`: Raw proof package bytes
- `value_tx.json`: Transaction template

### 3. Submit the Transaction

The transaction body should look like:

```json
{
  "call_message": {
    "set_value_with_proof": {
      "value": 42,
      "proof": "0x...",  // Hex-encoded proof package
      "gas": null
    }
  }
}
```

### 4. Verification on Chain

The module will:
1. Deserialize the proof package
2. Verify the proof against the method ID
3. Check `public_output.value == 42`
4. Set the value in state if all checks pass

## Security Considerations

### Code Commitment

The method ID is a SHA-256 hash of:
- WASM program bytes
- Packing parameter (e.g., 8192)

This ensures that only proofs from the exact approved program are accepted.

### Proof Verification

Currently, the verifier accepts proofs that were verified off-chain by the prover. In a production deployment, you would:
1. Implement on-chain verification using the Ligero verifier binary
2. Or batch-verify proofs using a ZK aggregation layer

### Admin Controls

Only the admin can update the method ID. This allows:
- Upgrading the guest program
- Fixing bugs in validation logic
- Changing constraints (e.g., from [0, 100] to [0, 200])

## Guest Program

The guest program (`value_validator.cpp`) is written in C++ and compiled to WASM. It:

```cpp
// 1. Read the value argument
uint32_t value;
memcpy(&value, arg(0), sizeof(uint32_t));

// 2. Validate constraints
if (value > 100) {
    assert_true(false, "Value out of range");
}

// 3. Commit public output
ValueProofPublic public_output { .value = value };
commit(&public_output, sizeof(ValueProofPublic));
```

## Development

### Running Tests

```bash
cargo test -p sov-value-setter-zk --all-features
```

### Integration with Rollup

Add to your runtime:

```rust
use sov_value_setter_zk::ValueSetterZk;

pub struct Runtime<S: Spec> {
    pub value_setter_zk: ValueSetterZk<S>,
    // ... other modules
}
```

Configure in genesis:

```toml
[value_setter_zk]
initial_value = 0
method_id = "0x..."  # Hash of value_validator.wasm + packing
admin = "0x..."
```

## Examples

### Set Value to 42

```rust
let proof_package = LigeroProofPackage {
    proof: raw_proof_bytes,
    public_output: ValueProofPublic { value: 42 },
};

let call_msg = CallMessage::SetValueWithProof {
    value: 42,
    proof: bincode::serialize(&proof_package)?.try_into()?,
    gas: None,
};
```

### Update Method ID (Admin)

```rust
let new_method_id = LigeroCodeCommitment::from_program_and_packing(
    &new_wasm_bytes,
    8192,
).encode();

let call_msg = CallMessage::UpdateMethodId {
    new_method_id: new_method_id.try_into()?,
};
```

## Future Enhancements

- [ ] Support multiple concurrent proofs per transaction
- [ ] Add batched proof verification
- [ ] Implement proof aggregation for efficiency
- [ ] Support configurable value ranges at runtime
- [ ] Add proof caching to reduce verification costs

## References

- [Ligero Paper](https://eprint.iacr.org/2022/1608)
- [Ligero Prover Repository](https://github.com/ligeroinc/ligero-prover)
- [Sovereign SDK Documentation](https://github.com/Sovereign-Labs/sovereign-sdk)
