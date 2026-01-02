# Midnight Privacy Transaction Generators

Tools for generating and submitting transactions to the Midnight Privacy shielded pool module.

## Overview

This directory contains three transaction generators:

1. **`midnight-deposit-generator`** - Creates deposit transactions to add funds to the shielded pool
2. **`midnight-tx-generator`** - Creates withdrawal/spend transactions with test parameters
3. **`withdraw-with-tree`** - Creates withdrawal transactions using real on-chain state

The complete **`deposit_and_withdraw.sh`** script orchestrates a full deposit + withdrawal flow.

## Quick Start

### Complete Deposit + Withdrawal Flow

From the repository root:

```bash
# Start the rollup (in a separate terminal)
cd examples/demo-rollup
cargo run --bin sov-demo-rollup -- \
  --da-layer mock \
  --rollup-config-path mock_rollup_config.toml \
  --genesis-paths examples/test-data/genesis/demo/mock

# Run the complete flow
./scripts/midnight-tx-generator/deposit_and_withdraw.sh
```

This will:
1. ✅ Deposit 100 tokens into the shielded pool (creates a note)
2. ✅ Generate a real Ligero ZK proof (~3MB, takes 0.3-0.5 seconds)
3. ✅ Withdraw 50 tokens to a transparent address
4. ✅ Keep 50 tokens as change in a new shielded note

### Custom Amounts

```bash
# Deposit 200, withdraw 150, keep 50 shielded
DEPOSIT_AMOUNT=200 WITHDRAW_AMOUNT=150 ./scripts/midnight-tx-generator/deposit_and_withdraw.sh
```

### With Proof Verifier Service

The script automatically sends transactions to both the sequencer and the proof-verifier service (if running):

```bash
# Start the proof-verifier service (in a separate terminal)
cd target/release
./proof-verifier \
  --bind 127.0.0.1:8080 \
  --node-rpc-url http://127.0.0.1:12346 \
  --signing-key-path ../../examples/test-data/keys/token_deployer_private_key.json \
  --da-connection-string "postgres://user:password@localhost/midnight_da"

# Run the script (sends to both endpoints automatically)
./scripts/midnight-tx-generator/deposit_and_withdraw.sh
```

The script behavior:
- ✅ Sends deposits to both sequencer and verifier service
- ✅ Sends withdrawals to both sequencer and verifier service
- ✅ Verifier verifies withdrawal proofs off-chain and caches results
- ✅ Verifier stores deposit data for monitoring
- ✅ Continues even if verifier service is not running

## Building

```bash
cd scripts/midnight-tx-generator
SKIP_GUEST_BUILD=1 cargo build --bin midnight-deposit-generator --bin midnight-tx-generator
```

## Individual Components

### 1. Deposit Generator

Creates a transaction that deposits tokens into the shielded pool:

```bash
cd scripts/midnight-tx-generator

# Generate deposit transaction
export DEPOSIT_AMOUNT=100
export NONCE=$(date +%s)
export PRIVATE_KEY_FILE=../../examples/test-data/keys/tx_signer_private_key.json

./target/debug/midnight-deposit-generator midnight_deposit_tx.bin

# Send to rollup
curl -X POST http://localhost:12346/sequencer/txs \
  -H 'Content-Type: application/json' \
  -d @midnight_deposit_tx.json
```

**Outputs:**
- `midnight_deposit_tx.bin` - Borsh-serialized transaction
- `midnight_deposit_tx.json` - Base64-encoded JSON payload
- `midnight_note_details.json` - Note parameters for later withdrawal

**Note Details:**
The deposit generator creates a note with random parameters:
- `rho` - Random nonce (ensures unique nullifier)
- `recipient` - Random recipient binding
- `nf_key` - Secret nullifier key (needed for spending)
- `commitment` - Note commitment hash
- `domain` - Module domain identifier (`[1u8; 32]`)

Save `midnight_note_details.json` to spend the note later!

### 2. Test Withdrawal Generator

Creates a withdrawal transaction using test parameters:

```bash
cd scripts/midnight-tx-generator

# This uses hardcoded test values from integration tests
./target/debug/midnight-tx-generator midnight_withdraw_tx.bin

# Send to rollup
curl -X POST http://localhost:12346/sequencer/txs \
  -H 'Content-Type: application/json' \
  -d @midnight_withdraw_tx.json
```

**Note:** This will likely fail with "Invalid anchor root" because the test note doesn't exist in the rollup's state. Use the complete `deposit_and_withdraw.sh` script instead.

### 3. Complete Flow Script

**`deposit_and_withdraw.sh`** orchestrates the entire privacy flow:

```bash
cd scripts/midnight-tx-generator
./deposit_and_withdraw.sh
```

**What it does:**

1. **Deposit Phase:**
   - Generates a note with fresh random parameters
   - Creates a deposit transaction
   - Sends to sequencer
   - Extracts the note position and anchor root from the response

2. **Withdrawal Phase:**
   - Loads note details from `midnight_note_details.json`
   - Builds a Merkle tree with the note at the correct position
   - Generates a real Ligero ZK proof (WebGPU-accelerated)
   - Creates withdrawal transaction
   - Sends to sequencer

**Environment Variables:**

```bash
DEPOSIT_AMOUNT=100              # Amount to deposit (default: 100)
WITHDRAW_AMOUNT=50              # Amount to withdraw (default: 50)
RECIPIENT=sov1v870par...        # Recipient address
PRIVATE_KEY_FILE=...            # Path to private key JSON

# Endpoint configuration
SEQUENCER_ENDPOINT=...          # Override sequencer URL (default: localhost:12346/sequencer/txs)
VERIFIER_ENDPOINT=...           # Override verifier URL (default: localhost:8080/midnight-privacy)
```

**How Endpoints Work:**

The script sends transactions as follows:
1. **Deposits** - Both sequencer (primary) and verifier service (secondary)
   - Sequencer response is used to extract position and anchor root
   - Verifier service stores deposit data for monitoring
2. **Withdrawals** - Both sequencer (primary) and verifier service (secondary)
   - Sequencer response is used to confirm transaction
   - Verifier service verifies proof off-chain and caches result

If the verifier service is not running, the script continues normally.

## Transaction Structure

### Deposit Transaction

```rust
RuntimeCall::MidnightPrivacy(
    CallMessage::Deposit {
        amount: u128,
        rho: Hash32,        // Random nonce
        recipient: Hash32,  // Random binding
        gas: Option<Gas>,
    }
)
```

### Withdrawal Transaction

```rust
RuntimeCall::MidnightPrivacy(
    CallMessage::Withdraw {
        proof: Vec<u8>,         // Ligero ZK proof (~3MB compressed)
        anchor_root: Hash32,    // Merkle root from deposit
        nullifier: Hash32,      // Prevents double-spending
        withdraw_amount: u128,  // Amount to transparent address
        to: Address,            // Recipient
        gas: Option<Gas>,
    }
)
```

## Ligero ZK Proofs

### Proof Generation

The withdrawal generator uses the Ligero ZK proof system:

- **Program:** `note_spend_guest.wasm` (circuit for note spending)
- **Prover:** WebGPU-accelerated prover binary
- **Size:** ~3MB compressed (gzip)
- **Time:** 0.3-0.5 seconds on modern hardware
- **Packing:** 8192 (FFT message packing size)

### Code Commitment

The proof is verified against a code commitment (method_id):

```
method_id = SHA-256(note_spend_guest.wasm || 8192_u32.to_le_bytes())
          = 02af46d4f30776e1d362cc07ac878bf948e840b76786325e3e782c96d3e08b36
```

This must match the `method_id` in the module's genesis configuration.

### Circuit Constraints

The ZK circuit proves:
1. ✅ Knowledge of a valid note (value, rho, recipient, nf_key)
2. ✅ Note exists in the Merkle tree (valid authentication path)
3. ✅ Correct nullifier derivation
4. ✅ Balance equation: `input_value = withdraw_amount + sum(output_values)`

## Privacy Model

### Shielded Pool

- **Notes:** Represent value in the shielded pool
- **Commitments:** Public note hashes stored in a Merkle tree
- **Nullifiers:** Prevent double-spending without revealing which note was spent
- **Zero-Knowledge:** Withdraw without revealing which note or how much remains

### Transaction Flow

```
Transparent → Shielded (Deposit)
  - Create note commitment
  - Add to Merkle tree
  - No proof required

Shielded → Transparent (Withdraw)
  - Prove knowledge of note
  - Reveal nullifier (unique per note)
  - Create change note (remaining value)
  - Requires ZK proof

Shielded → Shielded (Transfer)
  - Spend input note
  - Create output note(s)
  - withdraw_amount = 0
  - Requires ZK proof
```

## Output Files

### Generated by Deposit

- `midnight_deposit_tx.bin` - Borsh-serialized transaction
- `midnight_deposit_tx.json` - JSON payload for API
- `midnight_note_details.json` - Note parameters (SECRET! Contains `nf_key`)

### Generated by Withdrawal

- `midnight_withdraw_tx.bin` - Borsh-serialized transaction
- `midnight_withdraw_tx.json` - JSON payload for API
- `proof_data.gz` / `proof_data.bin` - Ligero proof (temporary). The exact filename depends on whether proof gzipping is enabled.

## Security Considerations

### Private Keys

The generators use test keys by default:
```bash
PRIVATE_KEY_FILE=../../examples/test-data/keys/tx_signer_private_key.json
```

**⚠️ For production:** Use secure key management and never commit private keys!

### Note Details

`midnight_note_details.json` contains the **nullifier key** which is the secret required to spend the note:

```json
{
  "domain": "0101010101...",
  "amount": 100,
  "rho": "random...",
  "recipient": "random...",
  "commitment": "d8aea9ab...",
  "nf_key": "SECRET_KEY"  // ⚠️ PRIVATE! Anyone with this can spend the note
}
```

**Keep this file secure!** It's equivalent to the private key for your shielded funds.

### Nullifier Reuse

Each note can only be spent once. The nullifier prevents double-spending:

```
nullifier = Hash(domain || nf_key || rho)
```

Once a nullifier is posted on-chain, that note cannot be spent again.

## Troubleshooting

### "Nullifier already spent"

**Cause:** You're trying to spend the same note twice.

**Solution:** Run the deposit again to create a fresh note with a new nullifier:
```bash
./deposit_and_withdraw.sh
```

Each run creates a note with fresh random `rho` and `nf_key`, giving a unique nullifier.

### "Invalid anchor root"

**Cause:** The Merkle root doesn't match the rollup's tree.

**Solution:** The complete script extracts the anchor root from the deposit response. If testing manually, query the rollup:
```bash
# Get current tree state
curl http://localhost:12346/state/midnight_privacy/current_root
```

### "Code commitment mismatch"

**Cause:** The proof was generated with a different WASM program than expected.

**Solution:** The verifier now auto-discovers the correct program based on the code commitment in the proof. Ensure:
1. The `note_spend_guest` circuit is available to `ligero-runner` (auto-discovery), or you set `LIGERO_PROGRAM_PATH` to a full path to the `.wasm`
2. Rollup genesis has the correct `method_id`: `02af46d4...`

### "Argument list too long" (curl error)

**Cause:** The 3MB transaction is too large for command-line arguments.

**Solution:** The scripts now use `-d @file.json` to read from file instead.

### Proof generation fails

**Cause:** WebGPU prover requires GPU access.

**Solutions:**
1. Run outside any sandbox that restricts GPU access
2. Check that shader files exist: `crates/adapters/ligero/bins/shader/`
3. Verify prover binary: `crates/adapters/ligero/bins/*/bin/webgpu_prover`

## Environment Requirements

### Required

- Rust toolchain (for building generators)
- `jq` (for JSON processing in scripts)
- `curl` (for sending transactions)

### Optional

- WebGPU-capable GPU (for real proof generation)
- `LIGERO_PROGRAM_PATH` env var (auto-discovery enabled if not set)

## Integration with Rollup

### Genesis Configuration

The rollup must be initialized with the correct `method_id`:

```json
{
  "method_id": [2, 175, 70, 212, 243, 7, 118, 225, ...],
  "domain": [1, 1, 1, 1, ...],
  "token_id": { "token_id": 1 }
}
```

### Endpoints

The script sends transactions to both endpoints:

#### Primary: Sequencer

- **Endpoint:** `POST http://localhost:12346/sequencer/txs`
  - Receives transaction directly
  - Returns transaction receipt with events
  - Script uses this response to extract position and anchor root

#### Secondary: Verifier Service

- **Endpoint:** `POST http://localhost:8080/midnight-privacy`
  - Receives both deposit and withdrawal transactions
  - For **deposits**: Verifies signature and stores data for monitoring
  - For **withdrawals**: Verifies proofs off-chain and caches results in database
  - Response is not used by the script
  - Script continues even if verifier is not running

#### State Queries

- **Module State:** `GET http://localhost:12346/state/midnight_privacy/{field}`
  - Query module state (tree size, roots, etc.)
  - Example: `/state/midnight_privacy/method_id`

## Development

### Adding New Generators

1. Create new binary in `src/`
2. Add `[[bin]]` entry to `Cargo.toml`
3. Build with `SKIP_GUEST_BUILD=1 cargo build --bin your-generator`

### Testing

```bash
# Run integration tests
cd ../../crates/module-system/module-implementations/midnight-privacy
cargo test --features native -- --nocapture

# Test deposit flow
cd ../../scripts/midnight-tx-generator
./target/debug/midnight-deposit-generator test_deposit.bin
```

## References

- **Module Implementation:** `crates/module-system/module-implementations/midnight-privacy/`
- **Integration Tests:** `crates/module-system/module-implementations/midnight-privacy/tests/integration/`
- **Ligero Adapter:** `crates/adapters/ligero/`
- **Guest WASM (artifact):** `<ligero-prover>/utils/circuits/bins/note_spend_guest.wasm`
- **Guest WAT (readable):** `<ligero-prover>/utils/circuits/bins/note_spend_guest.wat`
- **Genesis Config:** `examples/test-data/genesis/demo/mock/midnight_privacy.json`
