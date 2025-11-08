#![cfg(feature = "native")]

//! Tests for batched Merkle append functionality (Option A)
//!
//! These tests verify that:
//! 1. Individual transactions queue outputs without mutating the tree
//! 2. The end_rollup_block_hook applies all pending outputs at once
//! 3. Positions are assigned correctly in order
//! 4. The Merkle tree state is consistent after batched updates

use midnight_privacy::{CallMessage, ValueMidnightPrivacy, ValueSetterZkConfig};
use sov_modules_api::{
    hooks::BlockHooks, Context, ExecutionContext, Module, StateCheckpoint, WorkingSet,
};
use sov_state::Storage;
use sov_test_utils::TestSpec;

type C = sov_modules_api::default_spec::DefaultContext;

/// Helper to create a test module instance
fn create_test_module() -> ValueMidnightPrivacy<TestSpec> {
    ValueMidnightPrivacy::default()
}

/// Helper to create a sequencer context
fn create_sequencer_context() -> Context<TestSpec> {
    let sender = <TestSpec as sov_modules_api::Spec>::Address::from([1u8; 28]);
    let sequencer = <TestSpec as sov_modules_api::Spec>::Address::from([2u8; 28]);
    Context::new(sender, sequencer, ExecutionContext::Sequencer)
}

/// Initialize the module with test configuration
fn init_module(
    module: &mut ValueMidnightPrivacy<TestSpec>,
    working_set: &mut WorkingSet<TestSpec, C>,
) {
    let admin = <TestSpec as sov_modules_api::Spec>::Address::from([1u8; 28]);
    let token_id = sov_bank::TokenId::generate::<TestSpec>("test_token");
    
    let config = ValueSetterZkConfig::<TestSpec> {
        admin,
        method_id: [0u8; 32],
        tree_depth: 16,
        root_window_size: 100,
        domain: [0u8; 32],
        token_id,
        gas_per_output_append: None,
    };

    module.genesis(&config, working_set).unwrap();
}

#[test]
fn test_pending_log_starts_empty() {
    let tmpdir = tempfile::tempdir().unwrap();
    let storage = sov_state::ProverStorage::with_path(&tmpdir).unwrap();
    let mut working_set = WorkingSet::new(storage.clone());
    
    let mut module = create_test_module();
    init_module(&mut module, &mut working_set);
    
    // Check that pending_log starts empty
    let pending_log = module.pending_log.get(&mut working_set).unwrap();
    assert!(pending_log.is_none() || pending_log.unwrap().outputs.is_empty());
    
    // Check that pending_len is 0
    let pending_len = module.pending_len.get(&mut working_set).unwrap();
    assert_eq!(pending_len, Some(0));
}

#[test]
fn test_deposit_queues_output_without_tree_mutation() {
    let tmpdir = tempfile::tempdir().unwrap();
    let storage = sov_state::ProverStorage::with_path(&tmpdir).unwrap();
    let mut working_set = WorkingSet::new(storage.clone());
    
    let mut module = create_test_module();
    init_module(&mut module, &mut working_set);
    
    // Get initial tree state
    let initial_position = module.next_position.get(&mut working_set).unwrap().unwrap();
    let initial_tree = module.commitment_tree.get(&mut working_set).unwrap().unwrap();
    let initial_root = initial_tree.root();
    
    // Create a deposit (this should queue output, not mutate tree)
    let ctx = create_sequencer_context();
    let amount = 1000u128;
    let rho = [1u8; 32];
    let recipient = [2u8; 32];
    
    let call = CallMessage::Deposit {
        amount,
        rho,
        recipient,
        view_fvks: None,
        gas: None,
    };
    
    module.call(call, &ctx, &mut working_set).unwrap();
    
    // Verify that next_position has NOT changed (tree not mutated yet)
    let current_position = module.next_position.get(&mut working_set).unwrap().unwrap();
    assert_eq!(
        current_position, initial_position,
        "next_position should not change until hook runs"
    );
    
    // Verify that tree root has NOT changed
    let current_tree = module.commitment_tree.get(&mut working_set).unwrap().unwrap();
    let current_root = current_tree.root();
    assert_eq!(
        current_root, initial_root,
        "Tree root should not change until hook runs"
    );
    
    // Verify that pending_log has one entry
    let pending_log = module.pending_log.get(&mut working_set).unwrap().unwrap();
    assert_eq!(pending_log.outputs.len(), 1, "Should have 1 pending output");
    
    // Verify that pending_len is 1
    let pending_len = module.pending_len.get(&mut working_set).unwrap().unwrap();
    assert_eq!(pending_len, 1, "pending_len should be 1");
}

#[test]
fn test_end_block_hook_applies_pending_outputs() {
    let tmpdir = tempfile::tempdir().unwrap();
    let storage = sov_state::ProverStorage::with_path(&tmpdir).unwrap();
    let mut working_set = WorkingSet::new(storage.clone());
    
    let mut module = create_test_module();
    init_module(&mut module, &mut working_set);
    
    // Get initial state
    let initial_position = module.next_position.get(&mut working_set).unwrap().unwrap();
    let initial_tree = module.commitment_tree.get(&mut working_set).unwrap().unwrap();
    let initial_root = initial_tree.root();
    
    // Create multiple deposits
    let ctx = create_sequencer_context();
    for i in 0..3 {
        let call = CallMessage::Deposit {
            amount: 1000u128,
            rho: [i; 32],
            recipient: [i + 1; 32],
            view_fvks: None,
            gas: None,
        };
        module.call(call, &ctx, &mut working_set).unwrap();
    }
    
    // Verify tree hasn't changed yet
    let pre_hook_position = module.next_position.get(&mut working_set).unwrap().unwrap();
    assert_eq!(pre_hook_position, initial_position, "Tree not updated yet");
    
    // Verify we have 3 pending outputs
    let pending_log = module.pending_log.get(&mut working_set).unwrap().unwrap();
    assert_eq!(pending_log.outputs.len(), 3, "Should have 3 pending outputs");
    
    // Finalize working set and create checkpoint
    let (mut checkpoint, _, _) = working_set.finalize();
    let mut state_checkpoint = StateCheckpoint::with_witness(
        storage.clone(),
        checkpoint.freeze().2,
        &sov_kernels::basic::BasicKernel::<TestSpec>::default(),
    );
    
    // Run the end_rollup_block_hook
    module.end_rollup_block_hook(&mut state_checkpoint);
    
    // Verify tree HAS been updated
    let post_hook_position = module.next_position.get(&mut state_checkpoint).unwrap().unwrap();
    assert_eq!(
        post_hook_position,
        initial_position + 3,
        "Position should have advanced by 3"
    );
    
    // Verify tree root has changed
    let post_hook_tree = module.commitment_tree.get(&mut state_checkpoint).unwrap().unwrap();
    let post_hook_root = post_hook_tree.root();
    assert_ne!(
        post_hook_root, initial_root,
        "Tree root should have changed after hook"
    );
    
    // Verify pending_log is cleared
    let cleared_log = module.pending_log.get(&mut state_checkpoint).unwrap();
    assert!(
        cleared_log.is_none() || cleared_log.unwrap().outputs.is_empty(),
        "pending_log should be cleared"
    );
    
    // Verify pending_len is reset to 0
    let cleared_len = module.pending_len.get(&mut state_checkpoint).unwrap().unwrap();
    assert_eq!(cleared_len, 0, "pending_len should be reset to 0");
}

#[test]
fn test_multiple_blocks_with_batched_appends() {
    let tmpdir = tempfile::tempdir().unwrap();
    let storage = sov_state::ProverStorage::with_path(&tmpdir).unwrap();
    
    let mut module = create_test_module();
    
    // Block 1: 2 deposits
    {
        let mut working_set = WorkingSet::new(storage.clone());
        init_module(&mut module, &mut working_set);
        
        let ctx = create_sequencer_context();
        for i in 0..2 {
            let call = CallMessage::Deposit {
                amount: 1000u128,
                rho: [i; 32],
                recipient: [i + 1; 32],
                view_fvks: None,
                gas: None,
            };
            module.call(call, &ctx, &mut working_set).unwrap();
        }
        
        let (mut checkpoint, _, _) = working_set.finalize();
        let mut state_checkpoint = StateCheckpoint::with_witness(
            storage.clone(),
            checkpoint.freeze().2,
            &sov_kernels::basic::BasicKernel::<TestSpec>::default(),
        );
        
        module.end_rollup_block_hook(&mut state_checkpoint);
        
        let position = module.next_position.get(&mut state_checkpoint).unwrap().unwrap();
        assert_eq!(position, 2, "Should have 2 outputs after block 1");
    }
    
    // Block 2: 3 more deposits
    {
        let mut working_set = WorkingSet::new(storage.clone());
        
        let ctx = create_sequencer_context();
        for i in 2..5 {
            let call = CallMessage::Deposit {
                amount: 1000u128,
                rho: [i; 32],
                recipient: [i + 1; 32],
                view_fvks: None,
                gas: None,
            };
            module.call(call, &ctx, &mut working_set).unwrap();
        }
        
        let (mut checkpoint, _, _) = working_set.finalize();
        let mut state_checkpoint = StateCheckpoint::with_witness(
            storage.clone(),
            checkpoint.freeze().2,
            &sov_kernels::basic::BasicKernel::<TestSpec>::default(),
        );
        
        module.end_rollup_block_hook(&mut state_checkpoint);
        
        let position = module.next_position.get(&mut state_checkpoint).unwrap().unwrap();
        assert_eq!(position, 5, "Should have 5 total outputs after block 2");
    }
}

#[test]
fn test_hook_is_safe_with_empty_pending_log() {
    let tmpdir = tempfile::tempdir().unwrap();
    let storage = sov_state::ProverStorage::with_path(&tmpdir).unwrap();
    let mut working_set = WorkingSet::new(storage.clone());
    
    let mut module = create_test_module();
    init_module(&mut module, &mut working_set);
    
    let initial_position = module.next_position.get(&mut working_set).unwrap().unwrap();
    
    // Finalize without any deposits
    let (mut checkpoint, _, _) = working_set.finalize();
    let mut state_checkpoint = StateCheckpoint::with_witness(
        storage.clone(),
        checkpoint.freeze().2,
        &sov_kernels::basic::BasicKernel::<TestSpec>::default(),
    );
    
    // Run hook with empty pending_log (should be a no-op)
    module.end_rollup_block_hook(&mut state_checkpoint);
    
    // Verify nothing changed
    let final_position = module.next_position.get(&mut state_checkpoint).unwrap().unwrap();
    assert_eq!(final_position, initial_position, "Position should not change on empty hook");
}

