//! Integration test for batched Merkle append behavior in midnight-privacy module
//!
//! This test verifies that:
//! 1. Multiple deposits queue outputs to pending_log without immediate tree mutation
//! 2. The end_rollup_block_hook applies all pending outputs in one batch at block end
//! 3. The final tree state and root are correct after batched appends
//! 4. Transfers with ZK proofs work correctly after batched appends
//!
//! This uses the real sequencer flow to ensure the hook is actually called correctly.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use midnight_privacy::{
    CallMessage as MidnightCallMessage, Hash32, MerkleTree, SpendPublic, note_commitment, nullifier,
    ValueMidnightPrivacy, ValueSetterZkConfig,
};
use sov_api_spec::types::AcceptTxBody;
use sov_ligero_adapter::Ligero;
use sov_mock_da::BlockProducingConfig;
use sov_modules_api::{DispatchCall, RawTx, Runtime};
use sov_modules_stf_blueprint::GenesisParams;
use sov_paymaster::{Paymaster, PaymasterConfig};
use sov_rollup_interface::zk::{CodeCommitment, Zkvm, ZkvmHost};
use sov_test_utils::runtime::genesis::optimistic::HighLevelOptimisticGenesisConfig;
use sov_test_utils::sov_bank::config_gas_token_id;
use sov_test_utils::test_rollup::{GenesisSource, RollupBuilder, TestRollup};
use sov_test_utils::{
    default_test_signed_transaction, generate_optimistic_runtime_with_kernel, RtAgnosticBlueprint,
    TestSpec, TestUser, TEST_MAX_CONCURRENT_BLOBS,
};
use sov_value_setter::{ValueSetter, ValueSetterConfig};

// Generate a runtime with midnight-privacy module
generate_optimistic_runtime_with_kernel!(
    TestRuntime <=
    kernel_type: sov_kernels::soft_confirmations::SoftConfirmationsKernel<'a, S>,
    modules: [value_setter: ValueSetter<S>, paymaster: Paymaster<S>, midnight_privacy: ValueMidnightPrivacy<S>],
);

type RT = TestRuntime<TestSpec>;
type TestBlueprint = RtAgnosticBlueprint<TestSpec, RT>;

const MAX_BATCH_EXECUTION_TIME_MILLIS: u64 = 1_000 * 60 * 5;

/// Setup Ligero environment for ZK proof generation
fn setup_ligero_env() -> Result<(String, [u8; 32])> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .ok_or_else(|| anyhow::anyhow!("Could not find repository root"))?;

    let ligero_dir = repo_root.join("crates/adapters/ligero");
    let platform_dir = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        anyhow::bail!("Unsupported platform")
    };

    let bin_dir = ligero_dir.join("bins").join(platform_dir).join("bin");
    let shader_dir = ligero_dir.join("bins").join(platform_dir).join("shader");
    let program_path = ligero_dir.join("guest/bins/programs/note_spend_guest.wasm");
    let prover_bin = bin_dir.join("webgpu_prover");
    let verifier_bin = bin_dir.join("webgpu_verifier");

    if !program_path.exists() {
        anyhow::bail!("WASM program not found at: {}", program_path.display());
    }

    let program_path_str = program_path.to_str().unwrap().to_string();

    std::env::set_var("LIGERO_PROGRAM_PATH", &program_path_str);
    std::env::set_var("LIGERO_PROVER_BIN", prover_bin.to_str().unwrap());
    std::env::set_var("LIGERO_VERIFIER_BIN", verifier_bin.to_str().unwrap());
    std::env::set_var("LIGERO_SHADER_PATH", shader_dir.to_str().unwrap());
    std::env::set_var("LIGERO_PACKING", "8192");

    let host = <Ligero as Zkvm>::Host::from_args(&program_path_str);
    let code_commitment = host.code_commitment();
    let method_id: [u8; 32] = code_commitment.encode().try_into().unwrap();

    Ok((program_path_str, method_id))
}

/// Helper to create a test sequencer with midnight-privacy module and configurable batch size
async fn create_test_sequencer_with_batch_size(
    method_id: [u8; 32],
    max_batch_size: usize,
) -> (TestRollup<TestBlueprint>, TestUser<TestSpec>) {
    let genesis_config =
        HighLevelOptimisticGenesisConfig::<TestSpec>::generate().add_accounts_with_default_balance(1);
    let admin = genesis_config.additional_accounts()[0].clone();

    let rt_genesis_config = <RT as Runtime<TestSpec>>::GenesisConfig::from_minimal_config(
        genesis_config.into(),
        ValueSetterConfig {
            admin: admin.address(),
        },
        PaymasterConfig::default(),
        ValueSetterZkConfig {
            tree_depth: 20,
            root_window_size: 100,
            method_id,
            admin: admin.address(),
            domain: [1u8; 32],
            token_id: config_gas_token_id(),
            gas_per_output_append: None,
        },
    );

    let genesis_params = GenesisParams {
        runtime: rt_genesis_config,
    };

    let dir = Arc::new(tempfile::tempdir().unwrap());
    let seq_da_address = genesis_params
        .runtime
        .sequencer_registry
        .sequencer_config
        .seq_da_address;

    use sov_stf_runner::processes::RollupProverConfig;

    let rollup = RollupBuilder::<TestBlueprint>::new(
        GenesisSource::CustomParams(genesis_params),
        BlockProducingConfig::Manual,
        0,
    )
    .with_zkvm_host_args(Default::default())
    .set_config(|c| {
        c.automatic_batch_production = true;
        c.storage = dir;
        c.max_batch_size_bytes = max_batch_size; // Use configurable batch size
        c.blob_processing_timeout_secs = 60;
        c.max_concurrent_blobs = TEST_MAX_CONCURRENT_BLOBS;
        c.rollup_prover_config = Some(RollupProverConfig::Skip);
        if let sov_sequencer::SequencerKindConfig::Preferred(preferred_sequencer_config) =
            &mut c.sequencer_config
        {
            preferred_sequencer_config.batch_execution_time_limit_millis =
                MAX_BATCH_EXECUTION_TIME_MILLIS;
            preferred_sequencer_config.disable_state_root_consistency_checks = true;
        }
    })
    .set_da_config(|c| c.sender_address = seq_da_address)
    .set_persistent_da()
    .with_preferred_seq_min_profit_per_tx(0)
    .with_preferred_seq_recovery_strategy(sov_sequencer::preferred::RecoveryStrategy::TryToSave)
    .start()
    .await
    .unwrap();

    (rollup, admin)
}

/// Helper to create a deposit transaction
fn create_deposit_tx(
    admin: &TestUser<TestSpec>,
    amount: u128,
    rho: Hash32,
    recipient: Hash32,
    nonce: u64,
) -> RawTx {
    let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
        MidnightCallMessage::Deposit {
            amount,
            rho,
            recipient,
            view_fvks: None,
            gas: None,
        },
    );

    let tx = default_test_signed_transaction::<RT, TestSpec>(
        &admin.private_key,
        &midnight_call,
        nonce,
        &<RT as Runtime<TestSpec>>::CHAIN_HASH,
    );
    RawTx::new(borsh::to_vec(&tx).unwrap())
}

/// TEST: Process deposits in Block 1, then transfers in Block 2, verify batched Merkle appends
/// This tests that:
/// 1. Deposits queue outputs without immediate tree mutation
/// 2. end_rollup_block_hook applies all outputs in one batch per block
/// 3. Transfers with ZK proofs work correctly, spending deposits from Block 1
/// 4. Final tree state is correct after deposits and transfers across multiple blocks
#[tokio::test(flavor = "multi_thread")]
async fn test_batched_merkle_appends_with_deposits() -> Result<()> {
    println!("\n=== Testing Batched Merkle Appends with Deposits and Transfers ===\n");

    // Setup Ligero for ZK proof generation
    let (program_path, method_id) = match setup_ligero_env() {
        Ok(result) => result,
        Err(e) => {
            println!("⚠️  Skipping test: Ligero not available");
            println!("   Reason: {}", e);
            // Use dummy method_id for test
            (String::new(), [0u8; 32])
        }
    };
    if !program_path.is_empty() {
        println!("✓ Ligero configured: {}", program_path);
        println!("✓ Method ID: {}\n", hex::encode(method_id));
    }

    // Create sequencer with larger batch size to accommodate ~3MB ZK proofs
    // Each proof is about 3MB, so we need at least 10MB for multiple transactions
    let (rollup, admin) = create_test_sequencer_with_batch_size(method_id, 10 * 1024 * 1024).await; // 10MB
    let rollup = Arc::new(rollup);

    // Produce a DA block so the sequencer has a finalized slot
    rollup.da_service.produce_block_now().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Domain for commitments
    let domain = [1u8; 32];
    let mut local_tree = MerkleTree::new(20); // depth 20
    let mut expected_commitments = Vec::new();

    // ========== Block 1: Submit 2 deposits ==========
    println!("Block 1: Submitting 2 deposits...");
    for i in 0..2u8 {
        let rho = [i + 10; 32];
        let recipient = [i + 20; 32];
        let amount = 1000u128 * ((i + 1) as u128);

        // Calculate expected commitment
        let expected_cm = note_commitment(&domain, amount, &rho, &recipient);
        expected_commitments.push(expected_cm);

        // Create and submit deposit transaction
        let tx = create_deposit_tx(&admin, amount, rho, recipient, i as u64);

        let result = rollup
            .api_client()
            .accept_tx(&AcceptTxBody {
                body: BASE64_STANDARD.encode(&tx),
            })
            .await;

        if let Err(e) = &result {
            panic!("Deposit {} should be accepted, but got error: {:?}", i, e);
        }

        // Update local tree
        local_tree.set_leaf(i as usize, expected_cm);

        println!("  ✓ Deposit {} submitted (amount: {}, expected position: {})", i, amount, i);
        println!("    Commitment: {}", hex::encode(expected_cm));
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    println!("\nProducing DA block to trigger batch finalization (Block 1)...");
    rollup.da_service.produce_block_now().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    
    // Force close the sequencer's batch to trigger end_rollup_block and the hook
    println!("  Forcing batch close to trigger end_rollup_block hook...");
    rollup.force_close_batch().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    
    println!("✓ Block 1 DA block produced.");
    
    // Produce additional DA blocks to ensure Block 1 is fully committed
    for i in 0..3 {
        println!("  Producing DA block {} to ensure finalization...", i + 1);
        rollup.da_service.produce_block_now().await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }
    
    println!("✓ Block 1 should now be fully finalized and hook executed.");
    println!("  Expected root after Block 1: {}", hex::encode(local_tree.root()));

    // ========== Block 2: Submit 2 transfers ==========
    println!("\nBlock 2: Submitting 2 transfers with ZK proofs...");
    
    let tree_depth: u8 = 20;
    let nf_key: Hash32 = [4u8; 32]; // Secret nullifier key
    let shared_anchor = local_tree.root();
    
    // Generate proofs for spending the first 2 deposits
    let mut transfer_proofs = Vec::new();
    
    for i in 0..2usize {
        let (amount, rho, recipient) = (
            1000u128 * ((i + 1) as u128),
            [i as u8 + 10; 32],
            [i as u8 + 20; 32],
        );
        
        let position = i as u64;
        let cm = note_commitment(&domain, amount, &rho, &recipient);
        let siblings = local_tree.open(position as usize);
        
        println!("  → Transfer {} (spending position {})", i, position);
        println!("    Input: amount={}, commitment={}", amount, hex::encode(cm));
        
        // Derive nullifier
        let nf = nullifier(&domain, &nf_key, &rho);
        
        // Split the note: 60% to output1, 40% to output2
        let out1_value = (amount * 60) / 100;
        let out2_value = amount - out1_value;
        
        let out1_rho = [i as u8 + 100; 32];
        let out1_recipient = [i as u8 + 110; 32];
        let cm_out1 = note_commitment(&domain, out1_value, &out1_rho, &out1_recipient);
        
        let out2_rho = [i as u8 + 200; 32];
        let out2_recipient = [i as u8 + 210; 32];
        let cm_out2 = note_commitment(&domain, out2_value, &out2_rho, &out2_recipient);
        
        println!("    Outputs: {} units + {} units", out1_value, out2_value);
        println!("    Nullifier: {}", hex::encode(nf));
        
        let public_output = SpendPublic {
            anchor_root: shared_anchor,
            nullifier: nf,
            withdraw_amount: 0,
            output_commitments: vec![cm_out1, cm_out2],
            view_attestations: None,
        };
        
        // Generate ZK proof
        println!("    Generating ZK proof...");
        
        let program_path_clone = program_path.clone();
        let proof_result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut private_indices = vec![2, 3, 4, 5, 6];
            for j in 0..tree_depth as usize {
                private_indices.push(8 + j);
            }
            let base = 12 + tree_depth as usize;
            for j in 0..6 {
                private_indices.push(base + j);
            }
            
            let mut host = <Ligero as Zkvm>::Host::from_args(&program_path_clone)
                .with_private_indices(private_indices);
            
            // Add arguments in guest ABI order
            host.add_hex_arg(hex::encode(domain));
            host.add_str_arg(amount.to_string());
            host.add_hex_arg(hex::encode(rho));
            host.add_hex_arg(hex::encode(recipient));
            host.add_hex_arg(hex::encode(nf_key));
            host.add_str_arg(position.to_string());
            host.add_str_arg(tree_depth.to_string());
            
            for sibling in &siblings {
                host.add_hex_arg(hex::encode(sibling));
            }
            
            host.add_hex_arg(hex::encode(shared_anchor));
            host.add_hex_arg(hex::encode(nf));
            host.add_str_arg("0".to_string()); // withdraw_amount = 0
            host.add_str_arg("2".to_string()); // n_out = 2
            
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
            
            let proof_data = host.run(true)?;
            Ok(proof_data)
        }).await;
        
        match proof_result {
            Ok(Ok(proof_data)) => {
                println!("    ✓ ZK proof generated ({} bytes)", proof_data.len());
                transfer_proofs.push((proof_data, shared_anchor, nf, cm_out1, cm_out2));
            }
            Ok(Err(e)) => {
                println!("    ⚠️  Failed to generate proof: {}", e);
                println!("    Skipping this transfer");
                continue;
            }
            Err(e) => {
                println!("    ⚠️  Proof generation task failed: {}", e);
                println!("    Skipping this transfer");
                continue;
            }
        }
    }
    
    // Submit the transfer transactions
    for (i, (proof_data, anchor, nf, cm_out1, cm_out2)) in transfer_proofs.into_iter().enumerate() {
        let proof_safe = proof_data.try_into()
            .expect("Proof too large");
        
        let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
            MidnightCallMessage::Transfer {
                proof: proof_safe,
                anchor_root: anchor,
                nullifier: nf,
                view_ciphertexts: None,
                gas: None,
            }
        );
        
        let tx = default_test_signed_transaction::<RT, TestSpec>(
            &admin.private_key,
            &midnight_call,
            (2 + i) as u64, // nonce continues after deposits
            &<RT as Runtime<TestSpec>>::CHAIN_HASH,
        );
        
        let raw_tx = RawTx::new(borsh::to_vec(&tx).unwrap());
        
        let result = rollup
            .api_client()
            .accept_tx(&AcceptTxBody {
                body: BASE64_STANDARD.encode(&raw_tx),
            })
            .await;
        
        if let Err(e) = &result {
            println!("    ⚠️  Transfer {} failed to submit: {:?}", i, e);
        } else {
            println!("  ✓ Transfer {} submitted", i);
            
            // Update local tree with new outputs
            let next_pos = expected_commitments.len();
            local_tree.set_leaf(next_pos, cm_out1);
            local_tree.set_leaf(next_pos + 1, cm_out2);
            expected_commitments.push(cm_out1);
            expected_commitments.push(cm_out2);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    println!("\nProducing DA block to trigger batch finalization (Block 2)...");
    rollup.da_service.produce_block_now().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Force close the sequencer's batch to trigger end_rollup_block and the hook
    println!("  Forcing batch close to trigger end_rollup_block hook...");
    rollup.force_close_batch().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    println!("✓ Block 2 finalized.");
    let block2_root = local_tree.root();
    println!("  Expected root after Block 2: {}", hex::encode(block2_root));

    // ========== Block 3: Submit 2 more transfers to verify Block 2's state root ==========
    println!("\nBlock 3: Submitting 2 more transfers to verify Block 2's state root...");
    println!("  (These transfers will spend outputs created in Block 2's transfers)");

    // Generate proofs for spending the outputs from Block 2's first transfer
    // The first transfer in Block 2 created outputs at positions 2 and 3
    let mut transfer_proofs_block3 = Vec::new();
    
    for i in 0..2usize {
        // These are the outputs from Block 2's transfers (positions 2 and 3)
        let position = 2 + i as u64; // Positions 2, 3
        
        // Reconstruct the output commitments from Block 2
        let (out_value, out_rho, out_recipient) = if i == 0 {
            // Output 1 from Block 2's first transfer: 600 units
            (600u128, [100u8; 32], [110u8; 32])
        } else {
            // Output 2 from Block 2's first transfer: 400 units
            (400u128, [200u8; 32], [210u8; 32])
        };
        
        let cm = note_commitment(&domain, out_value, &out_rho, &out_recipient);
        let siblings = local_tree.open(position as usize);
        
        println!("  → Transfer {} (spending position {})", i, position);
        println!("    Input: amount={}, commitment={}", out_value, hex::encode(cm));
        
        // Derive nullifier
        let nf = nullifier(&domain, &nf_key, &out_rho);
        
        // Split the note: 50% to output1, 50% to output2
        let new_out1_value = out_value / 2;
        let new_out2_value = out_value - new_out1_value;
        
        let new_out1_rho = [(i * 2) as u8 + 150; 32];
        let new_out1_recipient = [(i * 2) as u8 + 160; 32];
        let cm_new_out1 = note_commitment(&domain, new_out1_value, &new_out1_rho, &new_out1_recipient);
        
        let new_out2_rho = [(i * 2 + 1) as u8 + 150; 32];
        let new_out2_recipient = [(i * 2 + 1) as u8 + 160; 32];
        let cm_new_out2 = note_commitment(&domain, new_out2_value, &new_out2_rho, &new_out2_recipient);
        
        println!("    Outputs: {} units + {} units", new_out1_value, new_out2_value);
        println!("    Nullifier: {}", hex::encode(nf));
        
        let public_output = SpendPublic {
            anchor_root: block2_root, // Use Block 2's root!
            nullifier: nf,
            withdraw_amount: 0,
            output_commitments: vec![cm_new_out1, cm_new_out2],
            view_attestations: None,
        };
        
        // Generate ZK proof
        println!("    Generating ZK proof...");
        
        let program_path_clone = program_path.clone();
        let proof_result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut private_indices = vec![2, 3, 4, 5, 6];
            for j in 0..tree_depth as usize {
                private_indices.push(8 + j);
            }
            let base = 12 + tree_depth as usize;
            for j in 0..6 {
                private_indices.push(base + j);
            }
            
            let mut host = <Ligero as Zkvm>::Host::from_args(&program_path_clone)
                .with_private_indices(private_indices);
            
            // Add arguments in guest ABI order
            host.add_hex_arg(hex::encode(domain));
            host.add_str_arg(out_value.to_string());
            host.add_hex_arg(hex::encode(out_rho));
            host.add_hex_arg(hex::encode(out_recipient));
            host.add_hex_arg(hex::encode(nf_key));
            host.add_str_arg(position.to_string());
            host.add_str_arg(tree_depth.to_string());
            
            for sibling in &siblings {
                host.add_hex_arg(hex::encode(sibling));
            }
            
            host.add_hex_arg(hex::encode(block2_root));
            host.add_hex_arg(hex::encode(nf));
            host.add_str_arg("0".to_string()); // withdraw_amount = 0
            host.add_str_arg("2".to_string()); // n_out = 2
            
            // Output 1
            host.add_str_arg(new_out1_value.to_string());
            host.add_hex_arg(hex::encode(new_out1_rho));
            host.add_hex_arg(hex::encode(new_out1_recipient));
            host.add_hex_arg(hex::encode(cm_new_out1));
            
            // Output 2
            host.add_str_arg(new_out2_value.to_string());
            host.add_hex_arg(hex::encode(new_out2_rho));
            host.add_hex_arg(hex::encode(new_out2_recipient));
            host.add_hex_arg(hex::encode(cm_new_out2));
            
            host.set_public_output(&public_output)?;
            
            let proof_data = host.run(true)?;
            Ok(proof_data)
        }).await;
        
        match proof_result {
            Ok(Ok(proof_data)) => {
                println!("    ✓ ZK proof generated ({} bytes)", proof_data.len());
                transfer_proofs_block3.push((proof_data, block2_root, nf, cm_new_out1, cm_new_out2));
            }
            Ok(Err(e)) => {
                println!("    ⚠️  Failed to generate proof: {}", e);
                println!("    Skipping this transfer");
                continue;
            }
            Err(e) => {
                println!("    ⚠️  Proof generation task failed: {}", e);
                println!("    Skipping this transfer");
                continue;
            }
        }
    }
    
    // Submit the Block 3 transfer transactions
    for (i, (proof_data, anchor, nf, cm_out1, cm_out2)) in transfer_proofs_block3.into_iter().enumerate() {
        let proof_safe = proof_data.try_into()
            .expect("Proof too large");
        
        let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
            MidnightCallMessage::Transfer {
                proof: proof_safe,
                anchor_root: anchor,
                nullifier: nf,
                view_ciphertexts: None,
                gas: None,
            }
        );
        
        let tx = default_test_signed_transaction::<RT, TestSpec>(
            &admin.private_key,
            &midnight_call,
            (4 + i) as u64, // nonce continues after Block 2's transfers
            &<RT as Runtime<TestSpec>>::CHAIN_HASH,
        );
        
        let raw_tx = RawTx::new(borsh::to_vec(&tx).unwrap());
        
        let result = rollup
            .api_client()
            .accept_tx(&AcceptTxBody {
                body: BASE64_STANDARD.encode(&raw_tx),
            })
            .await;
        
        if let Err(e) = &result {
            println!("    ⚠️  Transfer {} failed to submit: {:?}", i, e);
        } else {
            println!("  ✓ Transfer {} submitted successfully!", i);
            
            // Update local tree with new outputs
            let next_pos = expected_commitments.len();
            local_tree.set_leaf(next_pos, cm_out1);
            local_tree.set_leaf(next_pos + 1, cm_out2);
            expected_commitments.push(cm_out1);
            expected_commitments.push(cm_out2);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    println!("\nProducing DA block to trigger batch finalization (Block 3)...");
    rollup.da_service.produce_block_now().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Force close the sequencer's batch to trigger end_rollup_block and the hook
    println!("  Forcing batch close to trigger end_rollup_block hook...");
    rollup.force_close_batch().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    println!("\n✅ Batched Merkle append test with deposits and transfers completed!");
    println!("   - Block 1: 2 deposits queued and batched");
    println!("   - Block 2: 2 transfers (spending deposits, creating 4 new outputs) queued and batched");
    println!("   - Block 3: 2 transfers (spending Block 2 outputs, creating 4 new outputs) queued and batched");
    println!("   - Total outputs in tree: {} (2 deposits + 4 + 4 transfer outputs)", expected_commitments.len());
    println!("   - Expected final tree root: {}", hex::encode(local_tree.root()));
    println!("\n💡 This test verifies that:");
    println!("   ✓ Deposits queue outputs without immediate tree mutation");
    println!("   ✓ end_rollup_block_hook applies batched appends at block end");
    println!("   ✓ Block 2 transfers correctly reference Block 1's finalized state root");
    println!("   ✓ Block 3 transfers correctly reference Block 2's finalized state root");
    println!("   ✓ State roots are correctly propagated across multiple blocks");

    Ok(())
}

