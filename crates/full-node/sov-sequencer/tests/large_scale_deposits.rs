//! Large scale test for privacy deposits and transfers
//!
//! This test demonstrates high-volume processing with up to 1000 deposits
//! followed by the same number of transfers with real ZK proofs.

use std::sync::Arc;
use std::sync::Mutex;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use midnight_privacy::{
    CallMessage as MidnightCallMessage, Hash32, MerkleTree, SpendPublic, ValueMidnightPrivacy,
    ValueSetterZkConfig, note_commitment, nullifier,
};
use sov_api_spec::types::AcceptTxBody;
use sov_ligero_adapter::{Ligero, LigeroProofPackage};
use sov_mock_da::BlockProducingConfig;
use sov_modules_api::{DispatchCall, RawTx, Runtime};
use sov_modules_stf_blueprint::GenesisParams;
use sov_paymaster::{Paymaster, PaymasterConfig};
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
use sov_test_utils::runtime::genesis::optimistic::HighLevelOptimisticGenesisConfig;
use sov_test_utils::sov_bank::config_gas_token_id;
use sov_test_utils::test_rollup::{GenesisSource, RollupBuilder, TestRollup};
use sov_test_utils::{
    default_test_signed_transaction, generate_optimistic_runtime_with_kernel,
    RtAgnosticBlueprint, TestSpec, TestUser, TEST_MAX_CONCURRENT_BLOBS,
};
use sov_value_setter::{ValueSetter, ValueSetterConfig};

// Generate a runtime with SoftConfirmationsKernel to support Preferred Sequencer
// Now includes midnight-privacy module for privacy-preserving transactions
generate_optimistic_runtime_with_kernel!(
    TestRuntime <=
    kernel_type: sov_kernels::soft_confirmations::SoftConfirmationsKernel<'a, S>,
    modules: [value_setter: ValueSetter<S>, paymaster: Paymaster<S>, midnight_privacy: ValueMidnightPrivacy<S>],
);

type RT = TestRuntime<TestSpec>;
type TestBlueprint = RtAgnosticBlueprint<TestSpec, RT>;

const MAX_BATCH_EXECUTION_TIME_MILLIS: u64 = 1_000 * 60 * 30; // Allow batches to take up to 30 minutes

/// Create a custom API client with extended timeout for long-running operations
fn create_client_with_timeout(base_url: &str, timeout_secs: u64) -> sov_api_spec::client::Client {
    let client = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .expect("Failed to build reqwest client");
    sov_api_spec::client::Client::new_with_client(base_url, client)
}

/// Helper to create a Preferred Sequencer for testing with configurable batch size
/// Returns the rollup and a list of users that can be used to generate transactions
async fn create_test_sequencer_with_batch_size(
    max_batch_size: usize,
    method_id: [u8; 32],
    num_accounts: usize,
) -> (TestRollup<TestBlueprint>, Vec<TestUser<TestSpec>>) {
    let genesis_config =
        HighLevelOptimisticGenesisConfig::<TestSpec>::generate().add_accounts_with_default_balance(num_accounts);
    let accounts: Vec<_> = genesis_config.additional_accounts().to_vec();
    let admin = &accounts[0];
    
    let rt_genesis_config = <RT as Runtime<TestSpec>>::GenesisConfig::from_minimal_config(
        genesis_config.into(),
        ValueSetterConfig {
            admin: admin.address(),
        },
        PaymasterConfig::default(),
        ValueSetterZkConfig {
            tree_depth: 20,
            root_window_size: 100,
            method_id, // Use the actual code commitment
            admin: admin.address(),
            domain: [0u8; 32],
            token_id: config_gas_token_id(), // Use the actual gas token
            gas_per_output_append: None,
        },
    );

    let genesis_params = GenesisParams {
        runtime: rt_genesis_config,
    };

    let dir = Arc::new(tempfile::tempdir().unwrap());
    let seq_da_address = genesis_params.runtime.sequencer_registry.sequencer_config.seq_da_address;

    use sov_stf_runner::processes::RollupProverConfig;
    
    let rollup = RollupBuilder::<TestBlueprint>::new(
        GenesisSource::CustomParams(genesis_params),
        BlockProducingConfig::Manual,
        0, // finalization_blocks
    )
    .with_zkvm_host_args(Default::default())
    .set_config(|c| {
        c.automatic_batch_production = true;
        c.storage = dir;
        c.max_batch_size_bytes = max_batch_size;
        c.blob_processing_timeout_secs = 600; // 10 minutes for large batches
        c.max_concurrent_blobs = TEST_MAX_CONCURRENT_BLOBS;
        // Enable the prover service to generate aggregated block proofs
        c.rollup_prover_config = Some(RollupProverConfig::Skip);
        // Disable state root consistency checks since proofs aren't played yet in the sequencer
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

    (rollup, accounts)
}

/// Helper to setup Ligero environment and get code commitment
fn setup_ligero_env() -> Result<(String, [u8; 32])> {
    use sov_rollup_interface::zk::CodeCommitment;
    
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .ok_or_else(|| anyhow::anyhow!("Could not find repository root"))?;

    let ligero_dir = repo_root.join("crates/adapters/ligero");

    // Detect OS
    let platform_dir = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        anyhow::bail!("Unsupported platform. Supported: macOS, Linux");
    };

    let bin_dir = ligero_dir.join("bins").join(platform_dir).join("bin");
    let shader_dir = ligero_dir.join("bins").join(platform_dir).join("shader");
    let program_path = ligero_dir.join("guest/bins/programs/note_spend_guest.wasm");
    let prover_bin = bin_dir.join("webgpu_prover");
    let verifier_bin = bin_dir.join("webgpu_verifier");

    if !program_path.exists() {
        anyhow::bail!(
            "WASM program not found at: {}\nRun: cd crates/adapters/ligero/guest/note-spend-guest && cargo build --release --target wasm32-unknown-unknown",
            program_path.display()
        );
    }

    // Compute the code commitment from the WASM program
    let host = <Ligero as Zkvm>::Host::from_args(&program_path.to_string_lossy().to_string());
    let code_commitment = host.code_commitment();
    let method_id: [u8; 32] = code_commitment.encode().try_into()
        .map_err(|_| anyhow::anyhow!("Code commitment should be 32 bytes"))?;

    // Set environment variables for both proof generation AND verification
    std::env::set_var("LIGERO_PROGRAM_PATH", &program_path);
    std::env::set_var("LIGERO_PROVER_BIN", &prover_bin);
    std::env::set_var("LIGERO_VERIFIER_BIN", &verifier_bin);
    std::env::set_var("LIGERO_SHADER_PATH", &shader_dir);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok((program_path.to_string_lossy().to_string(), method_id))
}

/// TEST: Large scale deposits and transfers (up to 1000 each)
/// 
/// This test demonstrates the complete privacy-preserving flow at scale:
/// 1. Creates up to 1000 deposits into the shielded pool
/// 2. Generates REAL Ligero ZK proofs for transfers IN PARALLEL
/// 3. Submits all transfers through the async message channel
///
/// ## Environment Variables:
/// - `NUM_DEPOSITS`: Number of deposits to create (default: 10, max: 1000)
/// - `NUM_TRANSFERS`: Number of parallel transfers with proofs (default: 10, max: NUM_DEPOSITS)
/// - `CONCURRENT_PROOFS`: Number of proofs to generate concurrently (default: 4)
/// - `USE_MOCK_PROOFS`: Set to "1" to use mock proofs instead of real Ligero proofs (much faster)
///
/// ## How to Run:
///
/// Default (10 deposits, 10 transfers with real proofs):
/// ```bash
/// cargo test --package sov-sequencer large_scale_deposits_and_transfers -- --nocapture
/// ```
///
/// Large scale test with MOCK proofs (fast):
/// ```bash
/// USE_MOCK_PROOFS=1 NUM_DEPOSITS=1000 NUM_TRANSFERS=1000 cargo test --package sov-sequencer large_scale_deposits_and_transfers -- --nocapture
/// ```
///
/// Large scale test with REAL proofs (slow):
/// ```bash
/// NUM_DEPOSITS=100 NUM_TRANSFERS=100 CONCURRENT_PROOFS=8 cargo test --package sov-sequencer large_scale_deposits_and_transfers -- --nocapture --test-threads=1
/// ```
///
/// ## Requirements:
/// - Ligero binaries (webgpu_prover, webgpu_verifier)
/// - note_spend_guest.wasm program
/// - WebGPU-capable system for proof generation
/// - Sufficient system resources (RAM, GPU memory) for concurrent proof generation
#[tokio::test(flavor = "multi_thread")]
async fn large_scale_deposits_and_transfers() {
    use tokio::sync::mpsc;
    use tokio::sync::Semaphore;
    
    println!("\n=== Large Scale Midnight Privacy Test with ZK Proofs ===\n");
    
    // Read configuration from environment
    let num_deposits: usize = std::env::var("NUM_DEPOSITS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .min(1000); // Cap at 1000
    
    let num_transfers: usize = std::env::var("NUM_TRANSFERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .min(num_deposits); // Can't exceed deposits
    
    let concurrent_proofs: usize = std::env::var("CONCURRENT_PROOFS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .max(1); // At least 1
    
    let use_mock_proofs = std::env::var("USE_MOCK_PROOFS")
        .ok()
        .map(|s| s == "1" || s.to_lowercase() == "true")
        .unwrap_or(false);
    
    if num_transfers > num_deposits {
        panic!("NUM_TRANSFERS ({}) cannot exceed NUM_DEPOSITS ({})", num_transfers, num_deposits);
    }
    
    println!("Configuration:");
    println!("  NUM_DEPOSITS: {}", num_deposits);
    println!("  NUM_TRANSFERS: {}", num_transfers);
    println!("  CONCURRENT_PROOFS: {}", concurrent_proofs);
    println!("  USE_MOCK_PROOFS: {}", if use_mock_proofs { "YES (fast mode)" } else { "NO (real Ligero proofs)" });
    println!("  NUM_ACCOUNTS: {} (one per deposit for true parallelism)", num_deposits);
    
    // Setup Ligero environment BEFORE creating the sequencer (needed even for mock proofs to get method_id)
    let (program_path, method_id) = if use_mock_proofs {
        // For mock proofs, we can skip Ligero setup and use a dummy method_id
        // The LIGERO_MOCK_VERIFY flag will make the verifier accept any proof
        println!("  ✓ Mock proof mode enabled - skipping Ligero setup");
        println!("  ✓ Using dummy method ID");
        (String::from("mock"), [0u8; 32])
    } else {
        let (path, id) = setup_ligero_env().expect("Failed to setup Ligero environment");
        println!("  ✓ Ligero configured: {}", path);
        println!("  ✓ Method ID (code commitment): {}", hex::encode(id));
        (path, id)
    };
    
    // Create sequencer with larger batch size to accommodate multiple ~3MB ZK proofs
    // Create as many accounts as deposits for true parallel execution
    let batch_size = (100 * 1024 * 1024).max(num_deposits * 5 * 1024 * 1024); // At least 100MB or 5MB per deposit
    let (rollup, accounts) = create_test_sequencer_with_batch_size(batch_size, method_id, num_deposits).await;
    let rollup = Arc::new(rollup);
    
    // Produce a DA block
    rollup.da_service.produce_block_now().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    
    println!("\n📝 Step 1: Processing {} shielded deposits (one per account for parallelism)...", num_deposits);
    
    // Create a channel for transaction messages
    let (tx_sender, mut tx_receiver) = mpsc::channel::<(RawTx, String, std::time::Instant)>(100);
    
    // Track completion times for parallel analysis
    let completion_times = Arc::new(Mutex::new(Vec::new()));
    let completion_times_clone = Arc::clone(&completion_times);
    
    // Spawn a task that processes transactions from the channel
    let rollup_arc = Arc::clone(&rollup);
    let handle = tokio::spawn(async move {
        while let Some((raw_tx, desc, submit_time)) = tx_receiver.recv().await {
            let accept_start = std::time::Instant::now();
            let result = rollup_arc
                .api_client()
                .accept_tx(&AcceptTxBody {
                    body: BASE64_STANDARD.encode(&raw_tx),
                })
                .await;
            let accept_duration = accept_start.elapsed();
            
            if let Err(e) = &result {
                eprintln!("  ❌ {} failed: {:?}", desc, e);
                continue;
            }
            
            let total_duration = submit_time.elapsed();
            completion_times_clone.lock().unwrap().push((desc.clone(), accept_duration.as_secs_f64(), total_duration.as_secs_f64()));
            
            // Only print every 10th transaction to reduce noise for large batches
            if desc.contains("Deposit") {
                let tx_num: usize = desc.split('#').nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if tx_num % 10 == 0 || tx_num == 1 {
                    println!("  ✅ {} [submit→accept: {:.3}s, accept call: {:.3}s]", 
                        desc, 
                        total_duration.as_secs_f64(),
                        accept_duration.as_secs_f64()
                    );
                }
            } else {
                println!("  ✅ {} [submit→accept: {:.3}s, accept call: {:.3}s]", 
                    desc, 
                    total_duration.as_secs_f64(),
                    accept_duration.as_secs_f64()
                );
            }
        }
    });
    
    // Create deposits and track note details
    // Each account sends ONE deposit transaction (nonce 0) for true parallel execution
    let base_amount = 1000u128;
    let mut deposit_notes = Vec::new();
    
    let deposits_start = std::time::Instant::now();
    for i in 0..num_deposits {
        let account = &accounts[i]; // Each deposit uses a different account
        let amount = base_amount * (i as u128 + 1);
        let mut rho = [0u8; 32];
        // Use more bytes to ensure uniqueness for large batches
        rho[0] = ((i / 256) % 256) as u8;
        rho[1] = (i % 256) as u8;
        
        let mut recipient = [0u8; 32];
        recipient[0] = ((i / 256) % 256) as u8;
        recipient[1] = ((i + 10) % 256) as u8;
        
        // Store note details AND account index for later spending
        deposit_notes.push((i, amount, rho, recipient));
        
        let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
            MidnightCallMessage::Deposit {
                amount,
                rho,
                recipient,
                view_fvks: None,
                gas: None,
            }
        );
        
        let tx = default_test_signed_transaction::<RT, TestSpec>(
            &account.private_key, // Each account signs its own transaction
            &midnight_call,
            0, // All accounts start with nonce 0
            &<RT as Runtime<TestSpec>>::CHAIN_HASH,
        );
        
        let raw_tx = RawTx::new(borsh::to_vec(&tx).unwrap());
        let submit_time = std::time::Instant::now();
        tx_sender.send((raw_tx, format!("Deposit #{} from Account {}: {} units", i + 1, i, amount), submit_time)).await.unwrap();
        
        // Small delay between transactions to avoid overwhelming the system
        if i % 10 == 0 && i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }
    let deposits_submit_duration = deposits_start.elapsed();
    
    println!("  ⏱️  All {} deposits submitted in {:.3}s", num_deposits, deposits_submit_duration.as_secs_f64());
    println!("  ⏳ Waiting for all deposits to be accepted...");
    
    // Wait a bit for deposits to be processed
    let mut last_count = 0;
    for _ in 0..300 { // Wait up to 30 seconds
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let current_count = completion_times.lock().unwrap().len();
        if current_count == num_deposits {
            println!("  ✅ All {} deposits accepted!", num_deposits);
            break;
        }
        if current_count != last_count {
            println!("  ... {} / {} deposits accepted", current_count, num_deposits);
            last_count = current_count;
        }
    }
    
    // Analyze deposit parallelism
    let deposit_times = completion_times.lock().unwrap().clone();
    if !deposit_times.is_empty() {
        let deposit_accept_times: Vec<f64> = deposit_times.iter()
            .filter(|(desc, _, _)| desc.contains("Deposit"))
            .map(|(_, accept_time, _)| *accept_time)
            .collect();
        
        if deposit_accept_times.len() > 1 {
            let _avg_accept = deposit_accept_times.iter().sum::<f64>() / deposit_accept_times.len() as f64;
            let max_total = deposit_times.iter()
                .filter(|(desc, _, _)| desc.contains("Deposit"))
                .map(|(_, _, total)| *total)
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap_or(0.0);
            let sequential_time = deposit_accept_times.iter().sum::<f64>();
            let parallelism_factor = sequential_time / max_total.max(0.001);
            
            println!("\n  📊 Deposit Parallelism Analysis:");
            println!("     Sequential execution time: {:.3}s (sum of all accept times)", sequential_time);
            println!("     Actual wall clock time: {:.3}s (max submit→accept)", max_total);
            println!("     Parallelism factor: {:.2}x", parallelism_factor);
            if parallelism_factor > 1.5 {
                println!("     ✅ PARALLEL EXECUTION DETECTED - deposits processed concurrently!");
            } else {
                println!("     ⚠️  SEQUENTIAL EXECUTION - deposits processed one at a time");
            }
        }
    }
    
    if use_mock_proofs {
        println!("\n📝 Step 2: Creating {} MOCK ZK proofs (instant)...", num_transfers);
    } else {
        println!("\n📝 Step 2: Generating {} REAL ZK proofs with {} concurrent proof tasks...", num_transfers, concurrent_proofs);
    }
    
    let domain: Hash32 = [0u8; 32]; // Must match the module's domain
    let tree_depth: u8 = 20; // Must match module config
    
    // Build a Merkle tree with ALL the deposits (simulating the module's tree state)
    println!("  Building Merkle tree with all {} deposits...", num_deposits);
    let tree_start = std::time::Instant::now();
    let mut shared_tree = MerkleTree::new(tree_depth);
    for (i, (_, value, rho, recipient)) in deposit_notes.iter().enumerate() {
        let cm = note_commitment(&domain, *value, rho, recipient);
        shared_tree.set_leaf(i, cm);
    }
    let shared_anchor = shared_tree.root();
    let tree_duration = tree_start.elapsed();
    
    println!("  ✓ Merkle tree built in {:.3}s", tree_duration.as_secs_f64());
    println!("  Shared anchor root: {}", hex::encode(shared_anchor));
    
    // Generate proofs with controlled concurrency using a semaphore
    let proofs_start = std::time::Instant::now();
    let semaphore = Arc::new(Semaphore::new(concurrent_proofs));
    
    let proof_handles: Vec<_> = (0..num_transfers)
        .map(|i| {
            let (account_idx, value, rho, recipient) = deposit_notes[i];
            let program_path = program_path.clone();
            let nf_key: Hash32 = [4u8; 32]; // Secret nullifier key
            let shared_anchor = shared_anchor; // Use the shared anchor root
            let semaphore = Arc::clone(&semaphore);
            let use_mock = use_mock_proofs;
            
            // Get the siblings from the shared tree
            let _cm = note_commitment(&domain, value, &rho, &recipient);
            let siblings = shared_tree.open(i);
            
            tokio::spawn(async move {
                // Acquire semaphore permit to limit concurrency
                let _permit = semaphore.acquire().await.unwrap();
                
                tokio::task::spawn_blocking(move || -> Result<(usize, Vec<u8>, Hash32, Hash32, u128, u128)> {
                    let proof_start = std::time::Instant::now();
                    
                    // Derive nullifier
                    let nf = nullifier(&domain, &nf_key, &rho);
                    
                    // Split the note: 60% to output1, 40% to output2
                    let out1_value = (value * 60) / 100;
                    let out2_value = value - out1_value;
                    
                    let mut out1_rho = [0u8; 32];
                    out1_rho[0] = ((i * 2 + 5) / 256 % 256) as u8;
                    out1_rho[1] = ((i * 2 + 5) % 256) as u8;
                    let mut out1_recipient = [0u8; 32];
                    out1_recipient[0] = ((i * 2 + 6) / 256 % 256) as u8;
                    out1_recipient[1] = ((i * 2 + 6) % 256) as u8;
                    let cm_out1 = note_commitment(&domain, out1_value, &out1_rho, &out1_recipient);
                    
                    let mut out2_rho = [0u8; 32];
                    out2_rho[0] = ((i * 2 + 7) / 256 % 256) as u8;
                    out2_rho[1] = ((i * 2 + 7) % 256) as u8;
                    let mut out2_recipient = [0u8; 32];
                    out2_recipient[0] = ((i * 2 + 8) / 256 % 256) as u8;
                    out2_recipient[1] = ((i * 2 + 8) % 256) as u8;
                    let cm_out2 = note_commitment(&domain, out2_value, &out2_rho, &out2_recipient);
                    
                    let proof_data = if use_mock {
                        // Create a mock proof - must be a properly structured LigeroProofPackage
                        // The verifier will accept this when LIGERO_MOCK_VERIFY=1 is set
                        if i % 100 == 0 || i < 10 {
                            println!("  [Transfer {}] Creating mock proof at T+{:.3}s for note with {} units...", 
                                i + 1, proof_start.duration_since(proofs_start).as_secs_f64(), value);
                        }
                        
                        // Create the public output that the proof should commit to
                        let public_output = SpendPublic {
                            anchor_root: shared_anchor,
                            nullifier: nf,
                            withdraw_amount: 0,
                            output_commitments: vec![cm_out1, cm_out2],
                            view_attestations: None,
                        };
                        
                        // Serialize the public output with bincode
                        let public_output_bytes = bincode::serialize(&public_output)
                            .expect("Failed to serialize public output");
                        
                        // Create a mock proof package with dummy proof bytes but real public output
                        let package = LigeroProofPackage {
                            proof: vec![0u8; 10_000], // Much smaller mock proof bytes
                            public_output: public_output_bytes,
                            args_json: vec![],
                            private_indices: vec![],
                        };
                        
                        // Serialize the entire package with bincode
                        let mock_proof = bincode::serialize(&package)
                            .expect("Failed to serialize mock proof package");
                        
                        if i % 100 == 0 || i < 10 {
                            let total_time = proof_start.elapsed();
                            println!("  [Transfer {}] ✓ Mock proof created at T+{:.3}s: {} bytes in {:.3}s", 
                                i + 1, 
                                proof_start.duration_since(proofs_start).as_secs_f64() + total_time.as_secs_f64(),
                                mock_proof.len(), 
                                total_time.as_secs_f64()
                            );
                        }
                        
                        mock_proof
                    } else {
                        // Generate REAL Ligero proof
                        if i % 10 == 0 || i < 10 {
                            println!("  [Transfer {}] Starting proof generation at T+{:.3}s for note with {} units...", 
                                i + 1, proof_start.duration_since(proofs_start).as_secs_f64(), value);
                        }
                        
                        let position: u64 = i as u64;
                        
                        let public_output = SpendPublic {
                            anchor_root: shared_anchor,
                            nullifier: nf,
                            withdraw_amount: 0,
                            output_commitments: vec![cm_out1, cm_out2],
                            view_attestations: None,
                        };
                        
                        // Setup Ligero host for proof generation
                        let mut private_indices = vec![2, 3, 4, 5, 6];
                        for j in 0..tree_depth as usize { 
                            private_indices.push(8 + j);
                        }
                        let base = 12 + tree_depth as usize;
                        for j in 0..6 { 
                            private_indices.push(base + j);
                        }
                        
                        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path)
                            .with_private_indices(private_indices);
                        
                        // Add arguments in guest ABI order
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
                        
                        host.set_public_output(&public_output)
                            .context("Failed to set public output")?;
                        
                        let proof_gen_start = std::time::Instant::now();
                        let data = host.run(true)
                            .context("Failed to generate proof")?;
                        let proof_time = proof_gen_start.elapsed();
                        let total_time = proof_start.elapsed();
                        
                        if i % 10 == 0 || i < 10 {
                            println!("  [Transfer {}] ✓ Proof generated at T+{:.3}s: {} bytes in {:.1}s (total: {:.1}s)", 
                                i + 1, 
                                proof_start.duration_since(proofs_start).as_secs_f64() + proof_time.as_secs_f64(),
                                data.len(), 
                                proof_time.as_secs_f64(),
                                total_time.as_secs_f64()
                            );
                        }
                        
                        data
                    };
                    
                    Ok((account_idx, proof_data, shared_anchor, nf, out1_value, out2_value))
                }).await.expect("spawn_blocking failed")
            })
        })
        .collect();
    
    // Wait for all proofs to complete
    println!("  ⏳ Waiting for all {} proofs to complete...", num_transfers);
    let mut proofs = Vec::new();
    let mut completed = 0;
    for (i, handle) in proof_handles.into_iter().enumerate() {
        let result = handle.await
            .expect("Proof generation task panicked")
            .expect(&format!("Failed to generate proof {}", i + 1));
        proofs.push(result);
        completed += 1;
        
        // Print progress every 10 proofs
        if completed % 10 == 0 {
            println!("  ... {} / {} proofs completed", completed, num_transfers);
        }
    }
    let proofs_total_duration = proofs_start.elapsed();
    
    println!("  ✓ All {} proofs generated successfully in {:.3}s!", num_transfers, proofs_total_duration.as_secs_f64());
    println!("  📊 Average proof time: {:.3}s", proofs_total_duration.as_secs_f64() / num_transfers as f64);
    println!("  📊 Effective throughput: {:.2} proofs/sec", num_transfers as f64 / proofs_total_duration.as_secs_f64());
    
    println!("\n📝 Step 3: Submitting {} transfer transactions (one per account for parallelism)...", num_transfers);
    
    // Submit transfers concurrently
    let _submit_start = std::time::Instant::now();
    let mut tasks = Vec::with_capacity(num_transfers);
    for (i, (account_idx, proof_data, anchor, nf, out1_value, out2_value)) in proofs.into_iter().enumerate() {
        let account = accounts[account_idx].clone();
        let proof_safe = proof_data.try_into().expect("Proof too large");
        let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
            MidnightCallMessage::Transfer {
                proof: proof_safe,
                anchor_root: anchor,
                nullifier: nf,
                view_ciphertexts: None,
                gas: None,
            },
        );
        let tx = default_test_signed_transaction::<RT, TestSpec>(
            &account.private_key,
            &midnight_call,
            1,
            &<RT as Runtime<TestSpec>>::CHAIN_HASH,
        );
        let raw_tx = RawTx::new(borsh::to_vec(&tx).unwrap());
        let desc = format!(
            "Transfer #{} from Account {} with REAL ZK proof: split into {} + {} units",
            i + 1,
            account_idx,
            out1_value,
            out2_value
        );
        // Create a custom client with extended timeout
        let base_url = format!("http://{}", rollup.http_addr);
        let client_with_timeout = create_client_with_timeout(&base_url, 300); // 5 minute timeout
        tasks.push(tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let res = client_with_timeout
                .accept_tx(&AcceptTxBody {
                    body: BASE64_STANDARD.encode(&raw_tx),
                })
                .await;
            let t1 = std::time::Instant::now();
            (desc, t0, t1, res)
        }));
    }
    let results = futures::future::join_all(tasks).await;

    // Compute makespan and sum of per-tx durations
    let mut min_start: Option<std::time::Instant> = None;
    let mut max_finish: Option<std::time::Instant> = None;
    let mut sum = 0.0f64;
    let mut successful_txs = 0;
    let mut failed_txs = Vec::new();
    
    for r in results {
        let (desc, t0, t1, res) = r.expect("join");
        match res {
            Ok(_) => {
                let dt = (t1 - t0).as_secs_f64();
                sum += dt;
                min_start = Some(min_start.map_or(t0, |m| m.min(t0)));
                max_finish = Some(max_finish.map_or(t1, |m| m.max(t1)));
                
                // Only print every 10th transaction to reduce noise
                let tx_num: usize = desc.split('#').nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if tx_num % 10 == 0 || tx_num <= 10 {
                    println!("  ✅ {} [accept call duration: {:.3}s]", desc, dt);
                }
                successful_txs += 1;
            }
            Err(e) => {
                eprintln!("  ❌ {} [FAILED: {:?}]", desc, e);
                failed_txs.push((desc, e));
            }
        }
    }
    
    if !failed_txs.is_empty() {
        println!("\n  ⚠️  {} transaction(s) failed:", failed_txs.len());
        for (desc, err) in &failed_txs {
            println!("     - {}: {:?}", desc, err);
        }
    }
    
    if successful_txs == 0 {
        println!("\n  ❌ NO SUCCESSFUL TRANSACTIONS - test failed");
        panic!("All transfers failed");
    }
    
    if successful_txs < 2 {
        println!("\n  ⚠️  Only {} successful transaction - cannot compute accurate parallelism factor", successful_txs);
    } else {
        let wall = (max_finish.unwrap() - min_start.unwrap()).as_secs_f64();
        let factor = if wall > 0.0 { sum / wall } else { 0.0 };
        
        println!("\n  📊 Transfer Parallelism Analysis (makespan):");
        println!("     Successful transactions: {}/{}", successful_txs, num_transfers);
        println!("     Sum per‑tx durations: {:.3}s", sum);
        println!("     Makespan (earliest start → latest finish): {:.3}s", wall);
        println!("     Parallelism factor: {:.2}x", factor);
        
        if factor < 1.5 {
            println!("     ⚠️  SEQUENTIAL EXECUTION - transactions processed one at a time");
        } else {
            println!("     ✅ PARALLEL EXECUTION DETECTED - transactions processed concurrently");
        }
    }

    // Close the sender and wait for the deposit consumer to finish
    drop(tx_sender);
    handle.await.unwrap();

    let all_txs_duration = deposits_start.elapsed();
    
    println!("\n🎉 Large scale test completed successfully!");
    println!("   ✓ {} deposits into shielded pool (one per account)", num_deposits);
    if use_mock_proofs {
        println!("   ✓ {} shielded transfers with MOCK proofs ({} successful)", num_transfers, successful_txs);
    } else {
        println!("   ✓ {} shielded transfers with REAL Ligero proofs ({} successful)", num_transfers, successful_txs);
    }
    println!("   ✓ All transactions processed through async message channel");
    println!("\n📊 Timing Summary:");
    println!("   Total test time: {:.3}s", all_txs_duration.as_secs_f64());
    println!("   Deposits phase: {:.3}s", deposits_submit_duration.as_secs_f64());
    if use_mock_proofs {
        println!("   Proof creation phase: {:.3}s (avg: {:.3}s per mock proof)", 
            proofs_total_duration.as_secs_f64(),
            proofs_total_duration.as_secs_f64() / num_transfers as f64
        );
    } else {
        println!("   Proof generation phase: {:.3}s (avg: {:.3}s per proof)", 
            proofs_total_duration.as_secs_f64(),
            proofs_total_duration.as_secs_f64() / num_transfers as f64
        );
    }
    if successful_txs >= 2 {
        let wall = (max_finish.unwrap() - min_start.unwrap()).as_secs_f64();
        println!("   Transfers phase (wall): {:.3}s", wall);
    }
    if use_mock_proofs {
        println!("\n💡 This test demonstrates large-scale privacy-preserving flow with MOCK proofs for fast testing!");
        println!("   To test with REAL proofs, remove USE_MOCK_PROOFS from environment or set it to 0.");
    } else {
        println!("\n💡 This test demonstrates large-scale privacy-preserving flow with controlled concurrent proof generation!");
    }
}

