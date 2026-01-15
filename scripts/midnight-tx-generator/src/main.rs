#!/usr/bin/env rust-script
//! Generate a midnight withdrawal transaction with a REAL Ligero proof
//! Using exact parameters from the working integration test
//!
//! This creates a borsh-serialized transaction that can be sent to the sequencer.

use anyhow::{Context, Result};
use borsh;
use demo_stf::runtime::{Runtime, RuntimeCall};
use midnight_privacy::{
    nf_key_from_sk, note_commitment, nullifier, pk_from_sk, pk_ivk_from_sk, recipient_from_pk_v2,
    root_from_path, CallMessage, Hash32, MerkleTree, PrivacyAddress, SpendPublic,
};
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_ligero_adapter::Ligero;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_api::{CryptoSpec, PrivateKey, Spec};
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
use sov_rollup_ligero::MockDemoRollup;
use sov_test_utils::default_test_signed_transaction;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

mod note_spend_guest_v2;
mod rollup_schema;

// Type alias matching the rollup-ligero tests
type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

fn main() -> Result<()> {
    println!("=== Midnight Withdrawal Transaction Generator (Test-Based) ===\n");

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();

    let output_file = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from("midnight_withdraw_tx.bin")
    };

    // Get parameters from environment or use test defaults
    let withdraw_amount: u128 = std::env::var("WITHDRAW_AMOUNT")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .context("Invalid WITHDRAW_AMOUNT")?;

    let recipient_addr = std::env::var("RECIPIENT")
        .unwrap_or_else(|_| "sov1v870parxhssv5wyz634wqlt9yflrrnawlwzjhj8409q4yevcj3s".to_string());

    let nonce: u64 = std::env::var("NONCE")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .context("Invalid NONCE")?;

    // Note: The test uses a fixed note value of 100
    // If you need larger withdrawals, you'll need to create a note with more value
    let note_value: u128 = 100; // Fixed from test

    if withdraw_amount > note_value {
        anyhow::bail!(
            "Withdraw amount ({}) exceeds note value ({}). \n\
            The test-based generator uses a fixed note value of 100.\n\
            Please set WITHDRAW_AMOUNT to a value between 0 and 100.",
            withdraw_amount,
            note_value
        );
    }

    println!("Configuration:");
    println!("  Note value: {}", note_value);
    println!("  Withdraw amount: {}", withdraw_amount);
    println!(
        "  Change: {} (stays shielded)",
        note_value - withdraw_amount
    );
    println!("  Recipient: {}", recipient_addr);
    println!("  Nonce: {}\n", nonce);

    let node_url =
        std::env::var("NODE_API_URL").unwrap_or_else(|_| "http://localhost:12346".to_string());
    let chain_hash = rollup_schema::fetch_rollup_chain_hash(&node_url)?;
    println!(
        "  Chain hash (/rollup/schema): 0x{}\n",
        hex::encode(chain_hash)
    );

    // Parse the transparent destination once so we can bind it into the circuit input.
    let to_addr: <DemoRollupSpec as Spec>::Address = recipient_addr
        .parse()
        .context("Invalid recipient address")?;

    // Setup Ligero environment (discovers paths automatically)
    println!("Setting up Ligero environment...");
    let ligero_config = setup_ligero_env()?;
    println!("✓ Ligero configured");
    println!("  Program: {}", ligero_config.program);
    println!();

    // Use EXACT parameters from test_simple_note_spend test
    println!("Step 1: Creating note using test parameters...");

    let domain: Hash32 = [1u8; 32];
    let value: u64 = u64::try_from(note_value).context("NOTE_VALUE too large")?;
    let rho: Hash32 = [2u8; 32];
    let spend_sk: Hash32 = [4u8; 32];
    let pk_spend = pk_from_sk(&spend_sk);
    let pk_ivk = pk_ivk_from_sk(&domain, &spend_sk);
    let privacy_address = PrivacyAddress::from_keys(&pk_spend, &pk_ivk);
    let recipient: Hash32 = recipient_from_pk_v2(&domain, &pk_spend, &pk_ivk);
    let sender_id_in: Hash32 = recipient; // deposit-created note convention
    let nf_key: Hash32 = nf_key_from_sk(&domain, &spend_sk);

    println!("  Domain: 0x{}", hex::encode(&domain[..4]));
    println!("  Value: {}", value);
    println!("  Rho: 0x{}", hex::encode(&rho[..4]));
    println!("  Privacy address: {}", privacy_address);

    // Compute note commitment
    let cm = note_commitment(&domain, value, &rho, &recipient, &sender_id_in);
    println!("✓ Note commitment: 0x{}", hex::encode(&cm[..8]));

    // Build Merkle tree
    let tree_depth: u8 = 16;
    let mut tree = MerkleTree::new(tree_depth);
    let position: u64 = 0;
    tree.set_leaf(position as usize, cm);
    let anchor = tree.root();
    println!("✓ Merkle root: 0x{}", hex::encode(&anchor[..8]));

    // Get authentication path
    let siblings = tree.open(position as usize);

    // Verify path locally
    let computed_root = root_from_path(&cm, position, &siblings, tree_depth);
    assert_eq!(computed_root, anchor, "Merkle path verification failed!");
    println!("✓ Path verified\n");

    // Derive nullifier
    let nf = nullifier(&domain, &nf_key, &rho);
    println!("✓ Nullifier: 0x{}\n", hex::encode(&nf[..8]));

    // Change output (if any). For withdraw, change = value - withdraw_amount.
    let withdraw_amount_u64: u64 =
        u64::try_from(withdraw_amount).context("WITHDRAW_AMOUNT too large")?;
    let change_value: u64 = value
        .checked_sub(withdraw_amount_u64)
        .context("withdraw_amount exceeds note value")?;

    let out_rho: Hash32 = [9u8; 32];
    let sender_id_out: Hash32 = recipient; // spender identity
    let cm_out = if change_value > 0 {
        note_commitment(&domain, change_value, &out_rho, &recipient, &sender_id_out)
    } else {
        [0u8; 32]
    };

    let public_output = SpendPublic {
        anchor_root: anchor,
        // Filled below after fetching deny-map openings.
        blacklist_root: midnight_privacy::default_blacklist_root(),
        nullifier: nf,
        withdraw_amount,
        output_commitments: if change_value > 0 { vec![cm_out] } else { vec![] },
        view_attestations: None,
    };

    println!("Step 2: Generating proof (exactly like test)...");

    // note_spend_guest v2 ABI builder (includes inv_enforce + deny-map section).
    let input = note_spend_guest_v2::SpendInputV2 {
        value,
        rho,
        sender_id: sender_id_in,
        pos: position,
        siblings: siblings.clone(),
        nullifier: nf,
    };

    let mut outputs: Vec<note_spend_guest_v2::SpendOutputV2> = Vec::new();
    if change_value > 0 {
        outputs.push(note_spend_guest_v2::SpendOutputV2 {
            value: change_value,
            rho: out_rho,
            pk_spend,
            pk_ivk,
            cm: cm_out,
        });
    }

    let sender_addr = privacy_address;
    // The spend circuit's blacklist checks are:
    // - always: sender_id
    // - transfer only (withdraw_amount == 0): pay recipient (output 0)
    // Change outputs are enforced to be self in-circuit and are not checked separately.
    let addr_list = if withdraw_amount_u64 == 0 {
        // This generator uses a self-transfer shape when withdraw_amount == 0, so pay recipient is self.
        vec![sender_addr, sender_addr]
    } else {
        vec![sender_addr]
    };
    let (blacklist_root, deny_openings) =
        note_spend_guest_v2::fetch_deny_map_openings(&node_url, &addr_list)?;

    // Bind the transparent withdrawal destination into the statement.
    // The circuit requires:
    // - transfers (withdraw_amount == 0): withdraw_to == 0x00..00
    // - withdrawals (withdraw_amount  > 0): withdraw_to != 0x00..00
    let withdraw_to: Hash32 = if withdraw_amount_u64 == 0 {
        [0u8; 32]
    } else {
        note_spend_guest_v2::withdraw_to_from_address_bytes(to_addr.as_ref())?
    };

    let (args, private_indices) = note_spend_guest_v2::build_note_spend_args_v2(
        domain,
        spend_sk,
        pk_ivk,
        tree_depth,
        anchor,
        &[input],
        withdraw_amount_u64,
        withdraw_to,
        &outputs,
        blacklist_root,
        &deny_openings,
    )?;

    let mut host = <Ligero as Zkvm>::Host::from_args(&ligero_config.program)
        .with_packing(ligero_config.packing)
        .with_private_indices(private_indices);
    note_spend_guest_v2::add_args_to_host(&mut host, &args)?;

    let mut public_output = public_output;
    public_output.blacklist_root = blacklist_root;
    host.set_public_output(&public_output)?;

    println!("  Calling webgpu_prover...");
    let proof_start = Instant::now();
    let proof_bytes = host.run(true).context("Failed to generate proof")?;
    let proof_time = proof_start.elapsed();

    println!(
        "✓ Proof generated: {} bytes ({:.1}s)\n",
        proof_bytes.len(),
        proof_time.as_secs_f64()
    );

    // Load or generate private key
    let private_key = if let Ok(key_file) = std::env::var("PRIVATE_KEY_FILE") {
        println!("Loading private key from: {}", key_file);
        let key_data: PrivateKeyAndAddress<DemoRollupSpec> = serde_json::from_str(
            &fs::read_to_string(&key_file).context("Failed to read private key file")?,
        )?;
        key_data.private_key
    } else {
        println!("⚠ No PRIVATE_KEY_FILE set, generating random key");
        <<DemoRollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey::generate()
    };
    println!();

    // Step 3: Build the transaction
    println!("Step 3: Building signed transaction...");

    let proof_safe_vec = proof_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large"))?;

    let msg = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(CallMessage::Withdraw {
        proof: proof_safe_vec,
        anchor_root: anchor,
        nullifier: nf,
        withdraw_amount,
        to: to_addr,
        view_ciphertexts: None,
        gas: None,
    });

    let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
        default_test_signed_transaction(&private_key, &msg, nonce, &chain_hash);

    let tx_hash = tx.hash();
    println!("  ✓ Transaction hash: {}\n", tx_hash);

    // Serialize with borsh
    println!("Step 4: Serializing transaction...");
    let tx_bytes = borsh::to_vec(&tx).context("Failed to serialize transaction")?;

    println!("  ✓ Serialized size: {} bytes\n", tx_bytes.len());

    // Write to file
    fs::write(&output_file, &tx_bytes).context("Failed to write transaction file")?;

    println!("✓ Wrote transaction to: {}\n", output_file.display());

    // Also output base64 encoding for direct API use
    use base64::Engine;
    let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    let json_payload = serde_json::json!({
        "body": tx_base64
    });

    let json_file = output_file.with_extension("json");
    fs::write(&json_file, serde_json::to_string_pretty(&json_payload)?)
        .context("Failed to write JSON payload")?;

    println!("✓ Wrote JSON payload to: {}\n", json_file.display());

    println!("=== Success! ===");
    println!("\nTransaction details:");
    println!("  Anchor root: 0x{}", hex::encode(anchor));
    println!("  Nullifier: 0x{}", hex::encode(nf));
    println!("  Note value: {}", value);
    println!("  Withdraw amount: {} (transparent)", withdraw_amount);
    println!(
        "  Change: {} (stays in shielded pool)",
        change_value
    );

    Ok(())
}

#[derive(Debug)]
struct LigeroConfig {
    program: String,
    packing: u32,
}

fn setup_ligero_env() -> Result<LigeroConfig> {
    let config = LigeroConfig {
        // Pass a circuit name (or a full `.wasm` path) via LIGERO_PROGRAM_PATH.
        // `ligero-runner` resolves the correct wasm when given a circuit name.
        program: std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string()),
        packing: std::env::var("LIGERO_PACKING")
            .unwrap_or_else(|_| "8192".to_string())
            .parse()
            .context("Invalid LIGERO_PACKING")?,
    };

    // Set environment variables for Ligero
    std::env::set_var("LIGERO_PROGRAM_PATH", &config.program);
    std::env::set_var("LIGERO_PACKING", config.packing.to_string());

    Ok(config)
}
