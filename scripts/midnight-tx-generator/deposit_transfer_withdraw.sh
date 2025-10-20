#!/bin/bash
set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

GENERATOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$GENERATOR_DIR/../.." && pwd)"

# Get unique nonce based on timestamp
NONCE=$(date +%s)
DEPOSIT_AMOUNT="${DEPOSIT_AMOUNT:-1000}"
TRANSFER_OUT1="${TRANSFER_OUT1:-600}"
TRANSFER_OUT2="${TRANSFER_OUT2:-400}"
WITHDRAW_AMOUNT="${WITHDRAW_AMOUNT:-200}"
PRIVATE_KEY_FILE="${PRIVATE_KEY_FILE:-$REPO_ROOT/examples/test-data/keys/tx_signer_private_key.json}"
RECIPIENT="${RECIPIENT:-sov1v870parxhssv5wyz634wqlt9yflrrnawlwzjhj8409q4yevcj3s}"

# Endpoints - always send to sequencer (primary) and verifier service (secondary)
SEQUENCER_ENDPOINT="${SEQUENCER_ENDPOINT:-http://localhost:12346/sequencer/txs}"
VERIFIER_ENDPOINT="${VERIFIER_ENDPOINT:-http://localhost:8080/midnight-privacy}"

echo -e "${BLUE}=== Midnight Privacy: Full Lifecycle Demo ===${NC}\n"
echo "Flow:"
echo "  1. Deposit $DEPOSIT_AMOUNT (transparent → shielded)"
echo "  2. Transfer: Split into $TRANSFER_OUT1 + $TRANSFER_OUT2 (shielded → shielded)"
echo "  3. Withdraw $WITHDRAW_AMOUNT from first output (shielded → transparent)"
echo ""
echo "Parameters:"
echo "  Nonce: $NONCE"
echo "  Sequencer: $SEQUENCER_ENDPOINT"
echo "  Verifier: $VERIFIER_ENDPOINT"
echo ""

# Build generators if needed
if [ ! -f "$GENERATOR_DIR/target/debug/midnight-deposit-generator" ]; then
    echo -e "${YELLOW}Building generators...${NC}"
    cd "$GENERATOR_DIR"
    SKIP_GUEST_BUILD=1 cargo build --bin midnight-deposit-generator 2>&1 | grep -E "Compiling|Finished" || true
    cd "$REPO_ROOT"
    echo ""
fi

#############################################################################
# STEP 1: DEPOSIT - Put money INTO the privacy pool
#############################################################################
echo -e "${YELLOW}━━━ Step 1: Deposit ($DEPOSIT_AMOUNT tokens) ━━━${NC}"
export DEPOSIT_AMOUNT NONCE PRIVATE_KEY_FILE
cd "$GENERATOR_DIR"
"$GENERATOR_DIR/target/debug/midnight-deposit-generator" "midnight_deposit_tx.bin" > /tmp/deposit.log
cd "$REPO_ROOT"

# Send deposit to sequencer
DEPOSIT_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_deposit_tx.json" \
  "$SEQUENCER_ENDPOINT")

echo "$DEPOSIT_RESPONSE" | jq '.' 2>/dev/null || echo "$DEPOSIT_RESPONSE"

if echo "$DEPOSIT_RESPONSE" | grep -q '"status":400'; then
    echo -e "${RED}✗ Deposit failed${NC}"
    exit 1
fi

# Also send to verifier service
curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_deposit_tx.json" \
  "$VERIFIER_ENDPOINT" > /dev/null 2>&1 &

# Extract position and anchor root
NOTE_POSITION=$(echo "$DEPOSIT_RESPONSE" | jq -r '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.position')
ANCHOR_ROOT=$(echo "$DEPOSIT_RESPONSE" | jq -c '.events[] | select(.key == "ValueMidnightPrivacy/PoolDeposit") | .value.pool_deposit.new_root')

if [ -z "$NOTE_POSITION" ] || [ "$NOTE_POSITION" = "null" ]; then
    echo -e "${RED}✗ Failed to extract note position${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Deposit successful${NC}"
echo "  Created: Note@pos$NOTE_POSITION ($DEPOSIT_AMOUNT tokens)"
echo "  Anchor: $(echo $ANCHOR_ROOT | jq -r 'if type == "array" then (.[0:8] | map(tostring) | join(",")) else . end')"
echo ""

# Wait for confirmation
echo "Waiting 3 seconds..."
sleep 3
echo ""

#############################################################################
# STEP 2: TRANSFER - Move money WITHIN the privacy pool (pure shielded)
#############################################################################
echo -e "${YELLOW}━━━ Step 2: Transfer (split $DEPOSIT_AMOUNT → $TRANSFER_OUT1 + $TRANSFER_OUT2) ━━━${NC}"

TRANSFER_NONCE=$((NONCE + 1))

# Load note details
NOTE_DETAILS_FILE="$GENERATOR_DIR/midnight_note_details.json"
NOTE_DOMAIN=$(cat "$NOTE_DETAILS_FILE" | jq -r '.domain')
NOTE_VALUE=$(cat "$NOTE_DETAILS_FILE" | jq -r '.amount')
NOTE_RHO=$(cat "$NOTE_DETAILS_FILE" | jq -r '.rho')
NOTE_RECIPIENT=$(cat "$NOTE_DETAILS_FILE" | jq -r '.recipient')
NOTE_NF_KEY=$(cat "$NOTE_DETAILS_FILE" | jq -r '.nf_key')

# Create transfer generator (pure shielded with 2 outputs)
cat > /tmp/transfer_generator.rs << 'EOFRS'
use anyhow::{Context, Result};
use borsh;
use hex;
use demo_stf::runtime::{Runtime, RuntimeCall};
use midnight_privacy::{note_commitment, nullifier, CallMessage, Hash32, SpendPublic, MerkleTree};
use rand::Rng;
use serde_json;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_rollup_ligero::MockDemoRollup;
use sov_ligero_adapter::Ligero;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_api::{CryptoSpec, PrivateKey, Spec};
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
use sov_test_utils::default_test_signed_transaction;
use std::fs;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

mod demo_generated {
    include!("../../../examples/rollup-ligero/autogenerated.rs");
}

const CHAIN_HASH: [u8; 32] = demo_generated::CHAIN_HASH;

fn main() -> Result<()> {
    let domain: Hash32 = hex::decode(std::env::var("NOTE_DOMAIN")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid domain"))?;
    let value: u128 = std::env::var("NOTE_VALUE")?.parse()?;
    let rho: Hash32 = hex::decode(std::env::var("NOTE_RHO")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid rho"))?;
    let recipient: Hash32 = hex::decode(std::env::var("NOTE_RECIPIENT")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid recipient"))?;
    let nf_key: Hash32 = hex::decode(std::env::var("NOTE_NF_KEY")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid nf_key"))?;
    let out1_value: u128 = std::env::var("TRANSFER_OUT1")?.parse()?;
    let out2_value: u128 = std::env::var("TRANSFER_OUT2")?.parse()?;
    let position: u64 = std::env::var("NOTE_POSITION")?.parse()?;
    let nonce: u64 = std::env::var("NONCE")?.parse()?;
    
    let anchor_bytes: Vec<u8> = serde_json::from_str(&std::env::var("ANCHOR_ROOT")?)?;
    let anchor: Hash32 = anchor_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid anchor"))?;
    
    let cm = note_commitment(&domain, value, &rho, &recipient);
    let nf = nullifier(&domain, &nf_key, &rho);
    
    // Build tree with note at position
    let tree_depth: u8 = 16;
    let mut tree = MerkleTree::new(tree_depth);
    tree.set_leaf(position as usize, cm);
    let siblings = tree.open(position as usize);
    
    println!("Transfer: {} → {} + {}", value, out1_value, out2_value);
    println!("Input nullifier: 0x{}", hex::encode(&nf[..8]));
    
    // Create 2 output notes (pure shielded transfer, withdraw_amount = 0)
    let out1_rho: Hash32 = rand::thread_rng().gen();
    let out1_recipient: Hash32 = rand::thread_rng().gen();
    let cm_out1 = note_commitment(&domain, out1_value, &out1_rho, &out1_recipient);
    
    let out2_rho: Hash32 = rand::thread_rng().gen();
    let out2_recipient: Hash32 = rand::thread_rng().gen();
    let cm_out2 = note_commitment(&domain, out2_value, &out2_rho, &out2_recipient);
    
    // Save first output details for withdrawal step
    let out1_details = serde_json::json!({
        "domain": hex::encode(domain),
        "amount": out1_value,
        "rho": hex::encode(out1_rho),
        "recipient": hex::encode(out1_recipient),
        "nf_key": hex::encode(nf_key)
    });
    fs::write("midnight_transfer_out1_details.json", serde_json::to_string_pretty(&out1_details)?)?;
    
    let public_output = SpendPublic {
        anchor_root: anchor,
        nullifier: nf,
        withdraw_amount: 0,  // Pure shielded transfer
        output_commitments: vec![cm_out1, cm_out2],
    };
    
    let program_path = std::env::var("LIGERO_PROGRAM_PATH")?;
    let packing: u32 = std::env::var("LIGERO_PACKING")?.parse()?;
    
    let mut private_indices = vec![2, 3, 4, 5, 6];
    for i in 0..tree_depth as usize { private_indices.push(8 + i); }
    let base = 12 + tree_depth as usize;
    // Two outputs: mark all their fields as private
    for i in 0..6 { private_indices.push(base + i); }
    
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path)
        .with_packing(packing)
        .with_private_indices(private_indices);
    
    host.add_hex_arg(hex::encode(domain));
    host.add_str_arg(value.to_string());
    host.add_hex_arg(hex::encode(rho));
    host.add_hex_arg(hex::encode(recipient));
    host.add_hex_arg(hex::encode(nf_key));
    host.add_str_arg(position.to_string());
    host.add_str_arg(tree_depth.to_string());
    
    for sibling in &siblings {
        host.add_hex_arg(hex::encode(sibling));
    }
    
    host.add_hex_arg(hex::encode(anchor));
    host.add_hex_arg(hex::encode(nf));
    host.add_str_arg("0".to_string());  // withdraw_amount = 0
    host.add_str_arg("2".to_string());  // n_out = 2
    
    // Output 1
    host.add_str_arg(out1_value.to_string());
    host.add_hex_arg(hex::encode(out1_rho));
    host.add_hex_arg(hex::encode(out1_recipient));
    host.add_hex_arg(hex::encode(cm_out1));
    
    // Output 2
    host.add_str_arg(out2_value.to_string());
    host.add_hex_arg(hex::encode(out2_rho));
    host.add_hex_arg(hex::encode(out2_recipient));
    host.add_hex_arg(hex::encode(cm_out2));
    
    host.set_public_output(&public_output)?;
    
    println!("Generating proof...");
    let proof_bytes = host.run(true)?;
    println!("✓ Proof generated: {} bytes", proof_bytes.len());
    
    let key_data: PrivateKeyAndAddress<DemoRollupSpec> =
        serde_json::from_str(&fs::read_to_string(std::env::var("PRIVATE_KEY_FILE")?)?)?;
    
    let proof_safe = proof_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large"))?;
    
    let msg = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(
        CallMessage::Transfer {
            proof: proof_safe,
            anchor_root: anchor,
            nullifier: nf,
            gas: None,
        }
    );
    
    let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> = 
        default_test_signed_transaction(&key_data.private_key, &msg, nonce, &CHAIN_HASH);
    
    let tx_bytes = borsh::to_vec(&tx)?;
    fs::write("midnight_transfer_tx.bin", &tx_bytes)?;
    
    let tx_base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes);
    let tx_json = serde_json::json!({"body": tx_base64});
    
    fs::write("midnight_transfer_tx.json", serde_json::to_string_pretty(&tx_json)?)?;
    
    println!("✓ Transaction ready: midnight_transfer_tx.json");
    Ok(())
}
EOFRS

# Add transfer generator to Cargo.toml
cd "$GENERATOR_DIR"
if ! grep -q "transfer-generator" Cargo.toml; then
    cat >> Cargo.toml << 'EOF'

[[bin]]
name = "transfer-generator"
path = "/tmp/transfer_generator.rs"
EOF
fi

SKIP_GUEST_BUILD=1 cargo build --bin transfer-generator 2>&1 | grep -E "Compiling|Finished" || true

# Set environment and generate transfer
export NOTE_DOMAIN NOTE_VALUE NOTE_RHO NOTE_RECIPIENT NOTE_NF_KEY
export NOTE_POSITION ANCHOR_ROOT
export TRANSFER_OUT1 TRANSFER_OUT2
export NONCE=$TRANSFER_NONCE
export PRIVATE_KEY_FILE
export LIGERO_PROGRAM_PATH="${LIGERO_PROGRAM_PATH:-$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm}"
export LIGERO_PACKING="${LIGERO_PACKING:-8192}"

"$GENERATOR_DIR/target/debug/transfer-generator" 2>&1 | tee /tmp/transfer.log

# Send transfer to sequencer
TRANSFER_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_transfer_tx.json" \
  "$SEQUENCER_ENDPOINT")

echo "$TRANSFER_RESPONSE" | jq '.' 2>/dev/null || echo "$TRANSFER_RESPONSE"

if echo "$TRANSFER_RESPONSE" | grep -q '"status":400'; then
    echo -e "${RED}✗ Transfer failed${NC}"
    exit 1
fi

# Also send to verifier service
curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_transfer_tx.json" \
  "$VERIFIER_ENDPOINT" > /dev/null 2>&1 &

# Extract new positions from transfer response
TRANSFER_EVENTS=$(echo "$TRANSFER_RESPONSE" | jq -c '[.events[] | select(.key == "ValueMidnightPrivacy/NoteCreated")]')
OUT1_POSITION=$(echo "$TRANSFER_EVENTS" | jq -r '.[0].value.note_created.position')
OUT2_POSITION=$(echo "$TRANSFER_EVENTS" | jq -r '.[1].value.note_created.position')
TRANSFER_ROOT=$(echo "$TRANSFER_EVENTS" | jq -c '.[1].value.note_created.new_root')

echo -e "${GREEN}✓ Transfer successful${NC}"
echo "  Consumed: Note@pos$NOTE_POSITION ($DEPOSIT_AMOUNT tokens)"
echo "  Created: Note@pos$OUT1_POSITION ($TRANSFER_OUT1 tokens)"
echo "  Created: Note@pos$OUT2_POSITION ($TRANSFER_OUT2 tokens)"
echo "  Status: All value stays shielded (pure privacy)"
echo ""

# Wait for confirmation
echo "Waiting 3 seconds..."
sleep 3
echo ""

#############################################################################
# STEP 3: WITHDRAW - Take money OUT of the privacy pool
#############################################################################
echo -e "${YELLOW}━━━ Step 3: Withdraw ($WITHDRAW_AMOUNT from Note@pos$OUT1_POSITION) ━━━${NC}"

WITHDRAW_NONCE=$((NONCE + 2))

# Load first output details
OUT1_DETAILS_FILE="$GENERATOR_DIR/midnight_transfer_out1_details.json"
OUT1_DOMAIN=$(cat "$OUT1_DETAILS_FILE" | jq -r '.domain')
OUT1_VALUE=$(cat "$OUT1_DETAILS_FILE" | jq -r '.amount')
OUT1_RHO=$(cat "$OUT1_DETAILS_FILE" | jq -r '.rho')
OUT1_RECIPIENT=$(cat "$OUT1_DETAILS_FILE" | jq -r '.recipient')
OUT1_NF_KEY=$(cat "$OUT1_DETAILS_FILE" | jq -r '.nf_key')

# Create withdrawal generator
cat > /tmp/withdraw_generator.rs << 'EOFRS'
use anyhow::{Context, Result};
use borsh;
use hex;
use demo_stf::runtime::{Runtime, RuntimeCall};
use midnight_privacy::{note_commitment, nullifier, CallMessage, Hash32, SpendPublic, MerkleTree};
use rand::Rng;
use serde_json;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_rollup_ligero::MockDemoRollup;
use sov_ligero_adapter::Ligero;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_api::{CryptoSpec, PrivateKey, Spec};
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
use sov_test_utils::default_test_signed_transaction;
use std::fs;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

mod demo_generated {
    include!("../../../examples/rollup-ligero/autogenerated.rs");
}

const CHAIN_HASH: [u8; 32] = demo_generated::CHAIN_HASH;

fn main() -> Result<()> {
    let domain: Hash32 = hex::decode(std::env::var("OUT1_DOMAIN")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid domain"))?;
    let value: u128 = std::env::var("OUT1_VALUE")?.parse()?;
    let rho: Hash32 = hex::decode(std::env::var("OUT1_RHO")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid rho"))?;
    let recipient_hash: Hash32 = hex::decode(std::env::var("OUT1_RECIPIENT")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid recipient"))?;
    let nf_key: Hash32 = hex::decode(std::env::var("OUT1_NF_KEY")?)?
        .try_into().map_err(|_| anyhow::anyhow!("Invalid nf_key"))?;
    let withdraw_amount: u128 = std::env::var("WITHDRAW_AMOUNT")?.parse()?;
    let position: u64 = std::env::var("OUT1_POSITION")?.parse()?;
    let nonce: u64 = std::env::var("NONCE")?.parse()?;
    let recipient_addr: String = std::env::var("RECIPIENT")?;
    
    let anchor_bytes: Vec<u8> = serde_json::from_str(&std::env::var("TRANSFER_ROOT")?)?;
    let anchor: Hash32 = anchor_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid anchor"))?;
    
    let cm = note_commitment(&domain, value, &rho, &recipient_hash);
    let nf = nullifier(&domain, &nf_key, &rho);
    
    // Build tree - note was inserted at OUT1_POSITION during transfer
    let tree_depth: u8 = 16;
    let mut tree = MerkleTree::new(tree_depth);
    tree.set_leaf(position as usize, cm);
    let siblings = tree.open(position as usize);
    
    println!("Withdraw: {} → {} (transparent) + {} (change)", value, withdraw_amount, value - withdraw_amount);
    println!("Input nullifier: 0x{}", hex::encode(&nf[..8]));
    
    // Create change note
    let change_value = value - withdraw_amount;
    let change_rho: Hash32 = rand::thread_rng().gen();
    let change_recipient: Hash32 = rand::thread_rng().gen();
    let cm_change = note_commitment(&domain, change_value, &change_rho, &change_recipient);
    
    let public_output = SpendPublic {
        anchor_root: anchor,
        nullifier: nf,
        withdraw_amount,
        output_commitments: vec![cm_change],
    };
    
    let program_path = std::env::var("LIGERO_PROGRAM_PATH")?;
    let packing: u32 = std::env::var("LIGERO_PACKING")?.parse()?;
    
    let mut private_indices = vec![2, 3, 4, 5, 6];
    for i in 0..tree_depth as usize { private_indices.push(8 + i); }
    let base = 12 + tree_depth as usize;
    private_indices.extend(&[base, base + 1, base + 2]);
    
    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path)
        .with_packing(packing)
        .with_private_indices(private_indices);
    
    host.add_hex_arg(hex::encode(domain));
    host.add_str_arg(value.to_string());
    host.add_hex_arg(hex::encode(rho));
    host.add_hex_arg(hex::encode(recipient_hash));
    host.add_hex_arg(hex::encode(nf_key));
    host.add_str_arg(position.to_string());
    host.add_str_arg(tree_depth.to_string());
    
    for sibling in &siblings {
        host.add_hex_arg(hex::encode(sibling));
    }
    
    host.add_hex_arg(hex::encode(anchor));
    host.add_hex_arg(hex::encode(nf));
    host.add_str_arg(withdraw_amount.to_string());
    host.add_str_arg("1".to_string());  // n_out = 1 (change)
    
    // Change output
    host.add_str_arg(change_value.to_string());
    host.add_hex_arg(hex::encode(change_rho));
    host.add_hex_arg(hex::encode(change_recipient));
    host.add_hex_arg(hex::encode(cm_change));
    
    host.set_public_output(&public_output)?;
    
    println!("Generating proof...");
    let proof_bytes = host.run(true)?;
    println!("✓ Proof generated: {} bytes", proof_bytes.len());
    
    let key_data: PrivateKeyAndAddress<DemoRollupSpec> =
        serde_json::from_str(&fs::read_to_string(std::env::var("PRIVATE_KEY_FILE")?)?)?;
    
    let proof_safe = proof_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large"))?;
    
    let recipient_parsed = recipient_addr.parse()
        .map_err(|e| anyhow::anyhow!("Invalid recipient address: {}", e))?;
    
    let msg = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(
        CallMessage::Withdraw {
            proof: proof_safe,
            anchor_root: anchor,
            nullifier: nf,
            withdraw_amount,
            to: recipient_parsed,
            gas: None,
        }
    );
    
    let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> = 
        default_test_signed_transaction(&key_data.private_key, &msg, nonce, &CHAIN_HASH);
    
    let tx_bytes = borsh::to_vec(&tx)?;
    fs::write("midnight_withdraw_tx.bin", &tx_bytes)?;
    
    let tx_base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes);
    let tx_json = serde_json::json!({"body": tx_base64});
    
    fs::write("midnight_withdraw_tx.json", serde_json::to_string_pretty(&tx_json)?)?;
    
    println!("✓ Transaction ready: midnight_withdraw_tx.json");
    Ok(())
}
EOFRS

# Add withdraw generator to Cargo.toml
if ! grep -q "withdraw-generator" Cargo.toml; then
    cat >> Cargo.toml << 'EOF'

[[bin]]
name = "withdraw-generator"
path = "/tmp/withdraw_generator.rs"
EOF
fi

SKIP_GUEST_BUILD=1 cargo build --bin withdraw-generator 2>&1 | grep -E "Compiling|Finished" || true

# Set environment and generate withdrawal
export OUT1_DOMAIN OUT1_VALUE OUT1_RHO OUT1_RECIPIENT OUT1_NF_KEY
export OUT1_POSITION TRANSFER_ROOT
export WITHDRAW_AMOUNT RECIPIENT
export NONCE=$WITHDRAW_NONCE
export PRIVATE_KEY_FILE
export LIGERO_PROGRAM_PATH LIGERO_PACKING

"$GENERATOR_DIR/target/debug/withdraw-generator" 2>&1 | tee /tmp/withdraw.log

# Send withdrawal to sequencer
WITHDRAW_RESPONSE=$(curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_withdraw_tx.json" \
  "$SEQUENCER_ENDPOINT")

echo "$WITHDRAW_RESPONSE" | jq '.' 2>/dev/null || echo "$WITHDRAW_RESPONSE"

if echo "$WITHDRAW_RESPONSE" | grep -q '"status":400'; then
    echo -e "${RED}✗ Withdrawal failed${NC}"
    exit 1
fi

# Also send to verifier service
curl -s -4 -X POST \
  -H "Content-Type: application/json" \
  -d @"$GENERATOR_DIR/midnight_withdraw_tx.json" \
  "$VERIFIER_ENDPOINT" > /dev/null 2>&1 &

CHANGE_POSITION=$(echo "$WITHDRAW_RESPONSE" | jq -r '.events[] | select(.key == "ValueMidnightPrivacy/NoteCreated") | .value.note_created.position')

echo -e "${GREEN}✓ Withdrawal successful${NC}"
echo "  Consumed: Note@pos$OUT1_POSITION ($TRANSFER_OUT1 tokens)"
echo "  Withdrew: $WITHDRAW_AMOUNT tokens (transparent to $RECIPIENT)"
echo "  Created: Note@pos$CHANGE_POSITION ($((TRANSFER_OUT1 - WITHDRAW_AMOUNT)) tokens change)"
echo ""

cd "$REPO_ROOT"

#############################################################################
# SUMMARY
#############################################################################
echo -e "${BLUE}=== Summary ===${NC}"
echo ""
echo "Full Privacy Lifecycle Completed:"
echo "  1. Deposit:  $DEPOSIT_AMOUNT → Note@pos$NOTE_POSITION"
echo "  2. Transfer: Note@pos$NOTE_POSITION($DEPOSIT_AMOUNT) → Note@pos$OUT1_POSITION($TRANSFER_OUT1) + Note@pos$OUT2_POSITION($TRANSFER_OUT2)"
echo "  3. Withdraw: Note@pos$OUT1_POSITION($TRANSFER_OUT1) → $WITHDRAW_AMOUNT(transparent) + Note@pos$CHANGE_POSITION($((TRANSFER_OUT1 - WITHDRAW_AMOUNT)))"
echo ""
echo "Final State:"
echo "  • Transparent balance: +$WITHDRAW_AMOUNT tokens (withdrawn)"
echo "  • Shielded pool: Note@pos$OUT2_POSITION($TRANSFER_OUT2) + Note@pos$CHANGE_POSITION($((TRANSFER_OUT1 - WITHDRAW_AMOUNT))) = $((TRANSFER_OUT2 + TRANSFER_OUT1 - WITHDRAW_AMOUNT)) tokens"
echo ""
echo -e "${GREEN}✓ All transactions successful!${NC}"

