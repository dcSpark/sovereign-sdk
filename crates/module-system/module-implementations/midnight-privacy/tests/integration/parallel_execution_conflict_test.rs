#![cfg(feature = "native")]
#![allow(clippy::unwrap_used)]

//! Test that verifies the parallel execution fix for the midnight-privacy module.
//!
//! Previously, when multiple transactions executed in parallel against the same initial state
//! snapshot, they would all read the same counter values and write to the same keys, causing
//! data loss.
//!
//! The fix uses commitment-based keys (unique per commitment) instead of counter-based keys.
//! This test verifies that all commitments are preserved even with parallel execution.

use midnight_privacy::{
    cache_pre_verified_spend, clear_pre_verified_spend, note_commitment, nullifier, CallMessage,
    Hash32, MidnightPrivacyConfig, PendingCommitmentKey, SpendPublic, ValueMidnightPrivacy,
};
use sov_modules_api::capabilities::mocks::MockKernel;
use sov_modules_api::hooks::BlockHooks;
use sov_modules_api::transaction::AuthenticatedTransactionData;
use sov_modules_api::Context;
use sov_modules_api::StateProvider;
use sov_modules_api::VersionReader;
use sov_modules_api::{ExecutionContext, Gas, Genesis, Module, Spec, StateCheckpoint, WorkingSet};
use sov_test_utils::storage::ForklessStorageManager;
use sov_test_utils::storage::SimpleStorageManager;
use sov_test_utils::{
    default_test_tx_details, new_test_gas_meter, validate_and_materialize, TestSpec,
    TestStorageSpec,
};

fn make_cm(domain: &Hash32, val: u128, rho_byte: u8, recipient_byte: u8) -> Hash32 {
    let rho = [rho_byte; 32];
    let rcpt = [recipient_byte; 32];
    note_commitment(domain, val.try_into().unwrap(), &rho, &rcpt, &rcpt)
}

fn make_nf(domain: &Hash32, nfkey_byte: u8, rho_byte: u8) -> Hash32 {
    let nf_key = [nfkey_byte; 32];
    let rho = [rho_byte; 32];
    nullifier(domain, &nf_key, &rho)
}

/// This test verifies that the parallel-safe storage pattern works correctly.
///
/// It simulates parallel execution by creating multiple WorkingSets from the same
/// checkpoint. Each WorkingSet executes a transfer independently, and we verify
/// that ALL commitments are preserved using the new hash-based storage.
#[test]
fn parallel_execution_preserves_all_commitments_with_new_storage() {
    // Storage and kernel setup
    let mut sm = SimpleStorageManager::<TestStorageSpec>::new();
    sm.genesis();

    let mut mp = ValueMidnightPrivacy::<TestSpec>::default();

    // Genesis config
    let admin = <TestSpec as Spec>::Address::from([0xAA; 28]);
    let domain = [0x11; 32];
    let method_id = [0u8; 32];
    let token_id = sov_bank::TokenId::generate::<TestSpec>("NATIVE");

    let cfg = MidnightPrivacyConfig::<TestSpec> {
        tree_depth: 8,
        root_window_size: 16,
        method_id,
        admin,
        domain,
        token_id,
    };

    // Run module genesis
    {
        let storage = sm.create_storage();
        let mut cp =
            StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
        let mut gs = cp.to_genesis_state_accessor::<ValueMidnightPrivacy<TestSpec>>(&cfg);
        Genesis::genesis(&mut mp, &Default::default(), &cfg, &mut gs).unwrap();
        let (cache_log, accessory_delta, witness) = cp.freeze();
        let (new_root, change_set) =
            validate_and_materialize(storage, cache_log, &witness, sm.current_root()).unwrap();
        drop(accessory_delta);
        sm.commit_change_set(change_set, new_root);
    }

    let storage = sm.create_storage();

    // Get initial state
    let (initial_next_position, initial_root) = {
        let cp =
            StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
        let scratchpad = cp.to_tx_scratchpad();
        let tx = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
        let gas_meter = new_test_gas_meter::<TestSpec>();
        let mut ws = WorkingSet::<TestSpec>::create_working_set(scratchpad, &tx, gas_meter);
        let pos = mp.next_position.get(&mut ws).unwrap().unwrap();
        let root = mp.commitment_tree.get(&mut ws).unwrap().unwrap().root();
        (pos, root)
    };

    println!("Initial next_position: {}", initial_next_position);
    println!("Initial root: {:?}", hex::encode(initial_root));

    // Prepare 3 different transfers, each with unique nullifier and output
    let num_parallel_txs = 3usize;
    let mut outputs = Vec::new();
    let mut pub_inputs = Vec::new();

    for i in 0..num_parallel_txs {
        let out = make_cm(&domain, 100 + i as u128, 0x20 + i as u8, 0x30 + i as u8);
        let nf = make_nf(&domain, 0x90 + i as u8, 0x20 + i as u8);
        outputs.push(out);

        let pub_input = SpendPublic {
            anchor_root: initial_root,
            nullifier: nf,
            withdraw_amount: 0,
            output_commitments: vec![out],
            view_attestations: None,
        };
        cache_pre_verified_spend(pub_input.clone());
        pub_inputs.push(pub_input);
    }

    // SIMULATE PARALLEL EXECUTION:
    // Create WorkingSets from the SAME checkpoint (same state snapshot)
    let sender = <TestSpec as Spec>::Address::from([0xA1; 28]);
    let sequencer = <TestSpec as Spec>::Address::from([0xA2; 28]);
    let sequencer_da_addr: <<TestSpec as Spec>::Da as sov_modules_api::DaSpec>::Address =
        Default::default();
    let ctx = Context::<TestSpec>::new(sender, Default::default(), sequencer, sequencer_da_addr);

    let mut all_tx_changes = Vec::new();

    for i in 0..num_parallel_txs {
        // Each parallel worker creates a fresh WorkingSet from the SAME storage snapshot
        let cp =
            StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
        let scratchpad = cp.to_tx_scratchpad();
        let tx = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
        let gas_meter = new_test_gas_meter::<TestSpec>();
        let mut ws = WorkingSet::<TestSpec>::create_working_set(scratchpad, &tx, gas_meter);

        println!("Worker {}: executing transfer...", i);

        // Execute the transfer
        mp.call(
            CallMessage::Transfer {
                proof: Default::default(),
                anchor_root: pub_inputs[i].anchor_root,
                nullifier: pub_inputs[i].nullifier,
                view_ciphertexts: None,
                gas: Some(<TestSpec as Spec>::Gas::zero()),
            },
            &ctx,
            &mut ws,
        )
        .unwrap();

        // Verify the commitment is stored with unique hash-based key
        let current_height = ws.rollup_height_to_access();
        let cm_key = PendingCommitmentKey {
            height: current_height.get(),
            commitment: outputs[i],
        };
        let stored_pos = mp
            .pending_commitments_by_hash
            .get(&cm_key, &mut ws)
            .unwrap();
        println!(
            "Worker {}: commitment stored at hash-based key with position {:?}",
            i, stored_pos
        );
        assert!(
            stored_pos.is_some(),
            "Commitment should be stored with unique hash key"
        );

        // Get the tx changes
        let (scratchpad_after, _, _) = ws.finalize();
        let tx_changes = scratchpad_after.tx_changes(ExecutionContext::Sequencer);
        all_tx_changes.push(tx_changes);
    }

    println!("\n=== APPLYING CHANGES FROM ALL WORKERS ===");

    // Apply all changes to a fresh checkpoint (simulating sequential merge of parallel results)
    let mut final_cp =
        StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());

    for (i, changes) in all_tx_changes.iter().enumerate() {
        println!("Applying changes from worker {}...", i);
        final_cp.apply_tx_changes(changes);
    }

    // Verify all commitments exist in hash-based storage before flush
    println!("\n=== VERIFYING HASH-BASED STORAGE BEFORE FLUSH ===");
    let current_height = final_cp.rollup_height_to_access();
    let mut found_in_hash_storage = 0;
    for (i, cm) in outputs.iter().enumerate() {
        let cm_key = PendingCommitmentKey {
            height: current_height.get(),
            commitment: *cm,
        };
        if mp
            .pending_commitments_by_hash
            .get(&cm_key, &mut final_cp)
            .unwrap()
            .is_some()
        {
            println!("Output {} found in hash storage", i);
            found_in_hash_storage += 1;
        } else {
            println!("Output {} NOT found in hash storage", i);
        }
    }

    // With the new storage pattern, ALL commitments should be in hash storage
    assert_eq!(
        found_in_hash_storage, num_parallel_txs,
        "All {} commitments should be preserved in hash-based storage, but only {} found",
        num_parallel_txs, found_in_hash_storage
    );
    println!(
        "SUCCESS: All {} commitments preserved in hash-based storage!",
        num_parallel_txs
    );

    // Run end_block_flush to process pending commitments
    println!("\n=== RUNNING END_BLOCK_FLUSH ===");
    mp.end_rollup_block_hook(&mut final_cp);

    // Check final state
    let final_next_position = mp.next_position.get(&mut final_cp).unwrap().unwrap();
    let final_tree = mp.commitment_tree.get(&mut final_cp).unwrap().unwrap();

    println!("\n=== FINAL STATE ===");
    println!("Initial next_position: {}", initial_next_position);
    println!("Final next_position: {}", final_next_position);
    println!(
        "Commitments added: {}",
        final_next_position - initial_next_position
    );

    // Count how many commitments are actually in the tree
    let mut found_in_tree = 0;
    for (i, expected_cm) in outputs.iter().enumerate() {
        let mut found = false;
        for pos in initial_next_position..final_next_position {
            let leaf = final_tree.leaf(pos as usize);
            if leaf == *expected_cm {
                found = true;
                found_in_tree += 1;
                println!("Output {} found in tree at position {}", i, pos);
                break;
            }
        }
        if !found {
            println!("Output {} NOT found in tree", i);
        }
    }

    // With the fix, ALL commitments should be in the tree
    assert_eq!(
        found_in_tree, num_parallel_txs,
        "All {} commitments should be in the tree after flush, but only {} found. \
        The parallel-safe storage fix should prevent data loss.",
        num_parallel_txs, found_in_tree
    );

    // Verify the final position matches expected
    assert_eq!(
        final_next_position,
        initial_next_position + num_parallel_txs as u64,
        "Final next_position should be initial + number of commitments"
    );

    println!("\n=== TEST PASSED ===");
    println!(
        "Parallel execution fix verified: all {} commitments preserved!",
        num_parallel_txs
    );

    // Clean up cache
    for pub_input in &pub_inputs {
        clear_pre_verified_spend(&pub_input.nullifier);
    }
}

/// Test that sequential execution still works correctly with the new slot-based storage.
#[test]
fn sequential_execution_works_with_slot_based_storage() {
    // Storage and kernel setup
    let mut sm = SimpleStorageManager::<TestStorageSpec>::new();
    sm.genesis();

    let mut mp = ValueMidnightPrivacy::<TestSpec>::default();

    // Genesis config
    let admin = <TestSpec as Spec>::Address::from([0xAA; 28]);
    let domain = [0x11; 32];
    let method_id = [0u8; 32];
    let token_id = sov_bank::TokenId::generate::<TestSpec>("NATIVE");

    let cfg = MidnightPrivacyConfig::<TestSpec> {
        tree_depth: 8,
        root_window_size: 16,
        method_id,
        admin,
        domain,
        token_id,
    };

    // Run module genesis
    {
        let storage = sm.create_storage();
        let mut cp =
            StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
        let mut gs = cp.to_genesis_state_accessor::<ValueMidnightPrivacy<TestSpec>>(&cfg);
        Genesis::genesis(&mut mp, &Default::default(), &cfg, &mut gs).unwrap();
        let (cache_log, accessory_delta, witness) = cp.freeze();
        let (new_root, change_set) =
            validate_and_materialize(storage, cache_log, &witness, sm.current_root()).unwrap();
        drop(accessory_delta);
        sm.commit_change_set(change_set, new_root);
    }

    let storage = sm.create_storage();
    let initial_root = {
        let cp =
            StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
        let scratchpad = cp.to_tx_scratchpad();
        let tx = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
        let gas_meter = new_test_gas_meter::<TestSpec>();
        let mut ws = WorkingSet::<TestSpec>::create_working_set(scratchpad, &tx, gas_meter);
        mp.commitment_tree.get(&mut ws).unwrap().unwrap().root()
    };

    // Prepare 3 transfers
    let num_txs = 3usize;
    let mut outputs = Vec::new();
    let mut pub_inputs = Vec::new();

    for i in 0..num_txs {
        let out = make_cm(&domain, 200 + i as u128, 0x40 + i as u8, 0x50 + i as u8);
        let nf = make_nf(&domain, 0xA0 + i as u8, 0x40 + i as u8);
        outputs.push(out);

        let pub_input = SpendPublic {
            anchor_root: initial_root,
            nullifier: nf,
            withdraw_amount: 0,
            output_commitments: vec![out],
            view_attestations: None,
        };
        cache_pre_verified_spend(pub_input.clone());
        pub_inputs.push(pub_input);
    }

    let sender = <TestSpec as Spec>::Address::from([0xA1; 28]);
    let sequencer = <TestSpec as Spec>::Address::from([0xA2; 28]);
    let sequencer_da_addr: <<TestSpec as Spec>::Da as sov_modules_api::DaSpec>::Address =
        Default::default();
    let ctx = Context::<TestSpec>::new(sender, Default::default(), sequencer, sequencer_da_addr);

    // Execute SEQUENTIALLY on the same WorkingSet (simulating non-parallel execution)
    let cp = StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
    let scratchpad = cp.to_tx_scratchpad();
    let tx = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
    let gas_meter = new_test_gas_meter::<TestSpec>();
    let mut ws = WorkingSet::<TestSpec>::create_working_set(scratchpad, &tx, gas_meter);

    let initial_pos = mp.next_position.get(&mut ws).unwrap().unwrap();

    for (i, pub_input) in pub_inputs.iter().enumerate() {
        mp.call(
            CallMessage::Transfer {
                proof: Default::default(),
                anchor_root: pub_input.anchor_root,
                nullifier: pub_input.nullifier,
                view_ciphertexts: None,
                gas: Some(<TestSpec as Spec>::Gas::zero()),
            },
            &ctx,
            &mut ws,
        )
        .unwrap();

        let current_pos = mp.next_position.get(&mut ws).unwrap().unwrap();
        println!("After tx {}: next_position = {}", i, current_pos);
    }

    // Finalize and run flush
    let (scratchpad_after, _, _) = ws.finalize();
    let mut final_cp = scratchpad_after.commit();
    mp.end_rollup_block_hook(&mut final_cp);

    // Verify all commitments are in tree
    let final_pos = mp.next_position.get(&mut final_cp).unwrap().unwrap();
    let final_tree = mp.commitment_tree.get(&mut final_cp).unwrap().unwrap();

    assert_eq!(
        final_pos - initial_pos,
        num_txs as u64,
        "Sequential execution should add {} commitments",
        num_txs
    );

    let mut found = 0;
    for cm in &outputs {
        for pos in initial_pos..final_pos {
            if final_tree.leaf(pos as usize) == *cm {
                found += 1;
                break;
            }
        }
    }

    assert_eq!(
        found, num_txs,
        "All {} commitments should be in tree",
        num_txs
    );
    println!(
        "Sequential execution test passed: {} commitments added",
        num_txs
    );

    // Clean up
    for pub_input in &pub_inputs {
        clear_pre_verified_spend(&pub_input.nullifier);
    }
}
