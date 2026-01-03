# MidnightPrivacy Module

A privacy-preserving shielded pool module using Ligero ZK proofs, inspired by Zcash's Sapling protocol.

## Overview

The MidnightPrivacy module provides a Zcash-style shielded pool that enables privacy-preserving transactions on the Sovereign SDK rollup. It uses:

- **Ligero ZK proofs**: WebGPU-accelerated proof generation and verification
- **Poseidon2 hashing**: Efficient cryptographic hashing for Merkle trees
- **PRF-based nullifiers**: Privacy-preserving double-spend prevention
- **Merkle tree commitments**: Efficient note membership proofs

## Architecture

### Transaction Types

#### 1. Deposit (Transparent → Shielded)

Moves transparent tokens into the shielded pool:

```rust,ignore
CallMessage::Deposit {
    amount: 1000,        // Amount to deposit
    rho: [random],       // Random nonce
    recipient: [hash],   // Recipient binding
    gas: Some(gas),
}
```

**What happens:**
- Transfers `amount` tokens from sender to module account
- Computes note commitment: `cm = poseidon2(domain, value, rho, recipient)`
- Adds commitment to Merkle tree
- Emits `PoolDeposit` and `NoteCreated` events

#### 2. Transfer (The Unified Privacy Transaction)

The core privacy-preserving operation that atomically:
1. Verifies a ZK proof
2. Consumes input note(s) by nullifier
3. Creates output note commitments (from proof)
4. Optionally withdraws transparent value

```rust,ignore
CallMessage::Transfer {
    proof: proof_bytes,           // Ligero proof (~8MB)
    anchor_root: [root],          // Historical Merkle root
    nullifier: [nf],              // Derived nullifier
    withdraw_amount: 200,         // 0 for pure shielded transfer
    to: Some(recipient_address),  // None if no withdrawal
    gas: Some(gas),
}
```

**What happens:**
1. Verifies proof against configured method ID
2. Validates anchor_root is in historical roots index
3. Checks nullifier hasn't been used (prevents double-spending)
4. **Adds all output commitments from proof to tree** (key feature!)
5. If `withdraw_amount > 0`, transfers tokens to recipient
6. Marks nullifier as spent
7. Emits `NoteSpent`, `NoteCreated` (for each output), and optionally `PoolWithdraw`

#### Transfer Examples

**Pure Shielded Transfer (2 outputs):**
```rust,ignore
// Input: 1000 units → Output: 600 + 400 units
Transfer {
    proof: proof_with_2_outputs,
    withdraw_amount: 0,
    to: None,  // No transparent withdrawal
    ...
}
```

**Partial Withdrawal:**
```rust,ignore
// Input: 1000 units → Output: 400 units + Withdraw: 600 units
Transfer {
    proof: proof_with_1_output,
    withdraw_amount: 600,
    to: Some(recipient),
    ...
}
```

**Full Withdrawal:**
```rust,ignore
// Input: 1000 units → Withdraw: 1000 units (no outputs)
Transfer {
    proof: proof_with_no_outputs,
    withdraw_amount: 1000,
    to: Some(recipient),
    ...
}
```

### The ZK Proof

The proof demonstrates (in zero-knowledge):

1. **Note ownership**: Knowledge of a note `(value, rho, recipient)` in the commitment tree
2. **Merkle membership**: Valid authentication path from commitment to anchor root
3. **Nullifier derivation**: `nullifier = PRF(domain, nf_key, rho)` using secret `nf_key`
4. **Value conservation**: `input_value = sum(output_values) + withdraw_amount`

**Public inputs:**
- `anchor_root`: Historical Merkle root
- `nullifier`: Derived nullifier
- `withdraw_amount`: Transparent withdrawal amount
- `output_commitments`: Array of output note commitments

**Private inputs (witness):**
- `value`, `rho`, `recipient`: Note opening
- `nf_key`: Secret nullifier key
- `pos`: Leaf position in tree
- `siblings`: Merkle authentication path

### Security Features

#### 1. Nullifier-Based Double-Spend Prevention

Each note can only be spent once. The nullifier is derived as:
```text
nullifier = PRF(domain, nf_key, rho)
```

Once a nullifier is consumed, it's permanently marked as spent.

#### 2. Long-Range Anchor Validation

The module maintains two root indices:
- **Recent roots** (VecDeque): Fast O(n) mempool checks for active transactions
- **All roots** (NOMT-backed StateMap): Permanent O(log N) validation for any historical root

This enables Zcash ZIP-221 style long-range anchors: proofs can reference any historical root, not just recent ones.

#### 3. Public Output Binding

To prevent "unbound journal" attacks, critical values are passed as explicit transaction fields:
- `anchor_root`
- `nullifier`  
- `withdraw_amount`

The guest program verifies these match its computed values, ensuring cryptographic binding.

#### 4. Value Conservation

The circuit enforces:
```text
input_value = sum(output_values) + withdraw_amount
```

This prevents:
- Value creation (inflation)
- Value burning (deflation)
- Over-withdrawal attacks

## Module State

- `commitment_tree`: Merkle tree of note commitments (Poseidon2-based)
- `next_position`: Next available tree position
- `nullifier_set`: Set of consumed nullifiers
- `recent_roots`: Circular buffer of recent Merkle roots (fast mempool checks)
- `root_window_size`: Size of recent roots window
- `all_roots`: Permanent NOMT-backed index of ALL historical roots
- `root_seq`: Monotonic sequence counter for root ordering
- `method_id`: Ligero method ID (code commitment) for proof verification
- `admin`: Administrator who can update method ID
- `domain`: Domain tag for note/hash operations
- `token_id`: Supported native token
- `bank`: Bank module for token transfers

## Events

- `NoteCreated`: Note commitment added to tree
- `NoteSpent`: Nullifier consumed
- `PoolDeposit`: Transparent tokens deposited
- `PoolWithdraw`: Transparent tokens withdrawn
- `AnchorRootRecorded`: Root added to permanent index
- `MethodIdUpdated`: Method ID updated by admin
- `NoteEncrypted`: Encrypted note for viewing keys

## Viewing Keys

The module supports Zcash-style viewing keys for auditing:

- **Full Viewing Key (FVK)**: 32-byte key for decrypting notes
- **EncryptedNote**: AEAD-encrypted note bound to its commitment

Viewers can:
1. Decrypt the note using their FVK
2. Recompute the commitment from decrypted values
3. Verify it matches the on-chain commitment

This prevents "trust me bro" scenarios where senders could lie about note values.

## Usage Example

```rust,ignore
// 1. Deposit 1000 tokens into shielded pool
let deposit_msg = CallMessage::Deposit {
    amount: 1000,
    rho: random_hash(),
    recipient: recipient_hash(),
    gas: Some(gas),
};

// 2. Later, spend the note to create 2 outputs (600 + 400)
let transfer_msg = CallMessage::Transfer {
    proof: generate_proof(
        input_note,     // 1000 units
        outputs: [
            (600, out1_rho, out1_recipient),
            (400, out2_rho, out2_recipient),
        ],
        withdraw_amount: 0,  // Pure shielded transfer
    ),
    anchor_root: historical_root,
    nullifier: derived_nullifier,
    withdraw_amount: 0,
    to: None,
    gas: Some(gas),
};

// 3. Later, spend one output with partial withdrawal
let transfer_msg = CallMessage::Transfer {
    proof: generate_proof(
        input_note,     // 600 units
        outputs: [(400, change_rho, change_recipient)],
        withdraw_amount: 200,
    ),
    anchor_root: historical_root,
    nullifier: derived_nullifier,
    withdraw_amount: 200,
    to: Some(recipient_address),
    gas: Some(gas),
};
```

## Development

### Running Tests

The test suite includes:
- ZK proof generation and verification tests
- Merkle tree operation tests
- Hash function tests
- Viewing key encryption/decryption tests

```bash
cargo test --features native
```

### Building the Guest Program

The Ligero guest program (`note_spend_guest.wasm`) must be available:

```bash
ls -lh <ligero-prover>/utils/circuits/bins/note_spend_guest.wasm
cargo build --release --target wasm32-unknown-unknown
```

## Comparison to Zcash

| Feature | MidnightPrivacy | Zcash Sapling |
|---------|----------------|---------------|
| Proof system | Ligero (transparent setup) | Groth16 (trusted setup) |
| Hash function | Poseidon2 | Poseidon |
| Nullifiers | PRF-based | PRF-based |
| Anchor roots | Long-range (ZIP-221) | Long-range (ZIP-221) |
| Multiple outputs | ✅ Up to 16 | ✅ Up to 2 |
| Viewing keys | ✅ FVK with AEAD | ✅ IVK + OVK |
| Value binding | Circuit-enforced | Circuit-enforced |

## License

See [LICENSE.md](../../LICENSE.md)
