# Value Setter ZK Module - Demo Rollup Integration Guide

The `value-setter-zk` module has been successfully integrated into the demo-rollup! This guide shows you how to use it.

## What Was Added

### 1. Runtime Integration
The module was added to the `Runtime` struct in `stf/src/runtime.rs`:
```rust
pub value_setter_zk: sov_value_setter_zk::ValueSetterZk<S>,
```

### 2. Genesis Configuration
Added genesis configuration support in `stf/src/genesis_config.rs`:
- `ValueSetterZkConfig` type import
- Path to `value_setter_zk.json` genesis file
- Configuration loading in `create_genesis_config`

### 3. Genesis File
Created `/examples/test-data/genesis/integration-tests/value_setter_zk.json`:
```json
{
  "initial_value": 0,
  "method_id": [0, 0, 0, 0, ...],  // 32 zero bytes (placeholder)
  "admin": "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"
}
```

## Using the Module

### Step 1: Generate a Ligero Proof (Off-Chain)

First, compile the guest program (if not already done):
```bash
cd crates/adapters/ligero/guest
mkdir build && cd build
emcmake cmake ..
emmake make
cp value_validator.wasm ../../bins/programs/
```

Generate a proof for a value (e.g., 42):
```bash
cd crates/adapters/ligero
cargo run --example generate_value_proof --features native -- 42
```

This creates:
- `value_proof.bin`: Serialized proof package
- `value_tx.json`: Transaction template

### Step 2: Update Genesis with Correct Method ID

After building the guest program, calculate and update the method ID:

```bash
# Get the SHA-256 hash of (WASM + packing)
# The proof generator outputs the commitment
cargo run --example generate_value_proof --features native -- 42
# Look for: "Code commitment: <hex>"
```

Update `examples/test-data/genesis/integration-tests/value_setter_zk.json`:
```json
{
  "initial_value": 0,
  "method_id": [/* array of 32 bytes from commitment */],
  "admin": "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"
}
```

### Step 3: Start the Demo Rollup

```bash
cd examples/demo-rollup

# With mock DA
cargo run --bin sov-demo-rollup -- \
    --da-layer mock \
    --rollup-config-path mock_rollup_config.toml \
    --genesis-config-dir ../test-data/genesis/integration-tests
```

### Step 4: Submit a Transaction

Using the generated `value_tx.json`, submit it to the rollup:

```bash
# Example using curl (adjust endpoint as needed)
curl -X POST http://localhost:12345/submit_transaction \
  -H "Content-Type: application/json" \
  -d @value_tx.json
```

Or use the CLI wallet:
```bash
# Using sov-cli
sov-demo-rollup-cli \
    --rpc-url http://localhost:12345 \
    value-setter-zk \
    set-value-with-proof \
    --value 42 \
    --proof $(cat value_proof.bin | xxd -p | tr -d '\n')
```

## Module RPC Endpoints

Once the rollup is running, the module exposes these endpoints:

### Query Current Value
```bash
curl http://localhost:12345/rpc/value_setter_zk/value
```

### Query Method ID
```bash
curl http://localhost:12345/rpc/value_setter_zk/method_id
```

### Query Admin
```bash
curl http://localhost:12345/rpc/value_setter_zk/admin
```

## Call Messages

### SetValueWithProof

Sets a new value with ZK proof verification:

```json
{
  "module": "value_setter_zk",
  "method": "set_value_with_proof",
  "args": {
    "value": 42,
    "proof": "0x...",  // Hex-encoded LigeroProofPackage
    "gas": null
  }
}
```

**Requirements:**
- Proof must verify against the configured `method_id`
- Public output in proof must match `value`
- Value must be within [0, 100]

### UpdateMethodId

Updates the method ID (admin only):

```json
{
  "module": "value_setter_zk",
  "method": "update_method_id",
  "args": {
    "new_method_id": [/* 32 bytes */]
  }
}
```

## Events

### ValueSetWithProof
Emitted when a value is successfully set:
```json
{
  "value_set_with_proof": {
    "value": 42
  }
}
```

### MethodIdUpdated
Emitted when the admin updates the method ID:
```json
{
  "method_id_updated": {
    "new_method_id": [/* 32 bytes */]
  }
}
```

## Testing

Run the module tests:
```bash
cargo test -p sov-value-setter-zk --all-features
```

Run integration tests with the demo rollup:
```bash
cd examples/demo-rollup
cargo test value_setter_zk --features test-utils
```

## Troubleshooting

### "method_id not configured"
- Check that `value_setter_zk.json` exists in your genesis directory
- Verify the genesis file is valid JSON

### "Proof verification failed"
- Ensure you compiled the guest program (`value_validator.wasm`)
- Verify the method ID in genesis matches your compiled program
- Check that the proof was generated with the same packing parameter (8192)

### "Value mismatch"
- The value in the proof's public output must match the requested value
- Regenerate the proof with the correct value

### "Value out of range"
- The guest program enforces values must be ≤ 100
- Choose a value between 0 and 100

## Architecture

```
┌─────────────────────────────────────┐
│        Off-Chain (User)             │
│  1. Compile value_validator.wasm    │
│  2. Generate proof with Ligero      │
│  3. Create transaction              │
└──────────────┬──────────────────────┘
               │ Submit TX
               ▼
┌─────────────────────────────────────┐
│      Demo Rollup (On-Chain)         │
│  1. value-setter-zk receives TX     │
│  2. Verify proof against method_id  │
│  3. Check public_output == value    │
│  4. Update state                    │
│  5. Emit event                      │
└─────────────────────────────────────┘
```

## Next Steps

1. **Customize the Guest Program**: Modify `value_validator.cpp` to enforce different constraints
2. **Add More Modules**: Follow this pattern to integrate other ZK-verified modules
3. **Implement On-Chain Verification**: Update `LigeroVerifier` to call the verifier binary
4. **Add Proof Batching**: Accept multiple proofs in a single transaction

## References

- [Value Setter ZK README](../../crates/module-system/module-implementations/sov-value-setter-zk/README.md)
- [Ligero Integration Guide](../../LIGERO_INTEGRATION.md)
- [Ligero Adapter](../../crates/adapters/ligero/README.md)
- [Guest Program](../../crates/adapters/ligero/guest/README.md)

