#![cfg(feature = "native")]
#![allow(clippy::unwrap_used)]

//! Tests that roots created within a block are invisible as anchors until the end-of-block flush,
//! and become valid anchors after the flush.

use midnight_privacy::{
    cache_pre_verified_spend, clear_pre_verified_spend, note_commitment, nullifier, CallMessage,
    Hash32, SpendPublic, ValueMidnightPrivacy, MidnightPrivacyConfig, PendingRootKey,
};
use sov_modules_api::hooks::BlockHooks;
use sov_modules_api::capabilities::mocks::MockKernel;
use sov_modules_api::{Gas, Genesis, Module, Spec, StateCheckpoint, WorkingSet};
use sov_modules_api::VersionReader;
use sov_modules_api::Context;
use sov_modules_api::transaction::AuthenticatedTransactionData;
use sov_modules_api::StateProvider;
use sov_test_utils::storage::ForklessStorageManager;
use sov_test_utils::storage::SimpleStorageManager;
use sov_test_utils::{
    default_test_tx_details, new_test_gas_meter, validate_and_materialize, TestSpec, TestStorageSpec,
};

fn make_cm(domain: &Hash32, val: u128, rho_byte: u8, recipient_byte: u8) -> Hash32 {
    let rho = [rho_byte; 32];
    let rcpt = [recipient_byte; 32];
    note_commitment(domain, val, &rho, &rcpt)
}

fn make_nf(domain: &Hash32, nfkey_byte: u8, rho_byte: u8) -> Hash32 {
    let nf_key = [nfkey_byte; 32];
    let rho = [rho_byte; 32];
    nullifier(domain, &nf_key, &rho)
}

#[test]
fn pending_roots_are_invisible_until_flush_and_then_become_valid_anchors() {
    // Storage and kernel setup
    let mut sm = SimpleStorageManager::<TestStorageSpec>::new();
    // Initialize JMT genesis
    sm.genesis();

    // Module under test
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

    // Run module genesis against a checkpoint, then materialize to storage
    {
        let storage = sm.create_storage();
        let mut cp = StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
        // Create a genesis accessor from the checkpoint and initialize module state
        let mut gs = cp.to_genesis_state_accessor::<ValueMidnightPrivacy<TestSpec>>(&cfg);
        Genesis::genesis(&mut mp, &Default::default(), &cfg, &mut gs).unwrap();
        // Commit the genesis writes
        let (cache_log, accessory_delta, witness) = cp.freeze();
        let (new_root, change_set) =
            validate_and_materialize(storage, cache_log, &witness, sm.current_root()).unwrap();
        // Accessory writes are handled by change_set; accessory_delta is ignored here intentionally
        drop(accessory_delta);
        sm.commit_change_set(change_set, new_root);
    }

    // Begin Block 1: create a WorkingSet (TxState) on top of committed storage
    let storage = sm.create_storage();
    let cp = StateCheckpoint::<TestSpec>::new(storage.clone(), &MockKernel::<TestSpec>::default());
    let scratchpad = cp.to_tx_scratchpad();
    let tx = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
    let gas_meter = new_test_gas_meter::<TestSpec>();
    let mut ws = WorkingSet::<TestSpec>::create_working_set(scratchpad, &tx, gas_meter);

    // Construct a Context
    let sender = <TestSpec as Spec>::Address::from([0xA1; 28]);
    let sequencer = <TestSpec as Spec>::Address::from([0xA2; 28]);
    let sequencer_da_addr: <<TestSpec as Spec>::Da as sov_modules_api::DaSpec>::Address = Default::default();
    let ctx = Context::<TestSpec>::new(
        sender,
        Default::default(), // Credentials
        sequencer,
        sequencer_da_addr,
    );

    // Initial root is present in recent_roots
    let initial_root = mp
        .commitment_tree
        .get(&mut ws)
        .unwrap()
        .unwrap()
        .root();
    let recent0 = mp.recent_roots.get(&mut ws).unwrap().unwrap();
    assert_eq!(recent0.len(), 1);
    assert_eq!(recent0.front().copied().unwrap(), initial_root);

    // Prepare a pre-verified TRANSFER anchored to the initial root, with two outputs.
    let out1 = make_cm(&domain, 123, 0x21, 0x31);
    let out2 = make_cm(&domain, 456, 0x22, 0x32);
    let nf1: Hash32 = make_nf(&domain, 0x99, 0x21);

    let pub1 = SpendPublic {
        anchor_root: initial_root,
        nullifier: nf1,
        withdraw_amount: 0,
        output_commitments: vec![out1, out2],
        view_attestations: None,
    };
    cache_pre_verified_spend(pub1.clone());

    // Act 1: Same-block transfer (uses pre-verified path). This should succeed.
    mp.call(
        CallMessage::Transfer {
            proof: Default::default(),
            anchor_root: pub1.anchor_root,
            nullifier: pub1.nullifier,
            view_ciphertexts: None,
            gas: Some(<TestSpec as Spec>::Gas::zero()),
        },
        &ctx,
        &mut ws,
    )
    .unwrap();

    // After transfer: roots were queued in pending_roots_indexed. recent_roots unchanged.
    let current_height = ws.rollup_height_to_access();
    let pend_count_after_t1 = mp
        .pending_roots_count
        .get(&current_height, &mut ws)
        .unwrap()
        .unwrap_or(0);
    assert_eq!(pend_count_after_t1, 2);
    let recent_after_t1 = mp.recent_roots.get(&mut ws).unwrap().unwrap();
    assert_eq!(recent_after_t1.len(), 1);
    assert_eq!(recent_after_t1.front().copied().unwrap(), initial_root);

    // The newest root in this block is NOT a valid anchor yet.
    let key = PendingRootKey {
        height: current_height.get(),
        idx: 1,
    };
    let same_block_root = mp
        .pending_roots_indexed
        .get(&key, &mut ws)
        .unwrap()
        .unwrap();

    // Prepare another pre-verified transfer anchored to same_block_root - must fail before flush.
    let nf2: Hash32 = make_nf(&domain, 0x9A, 0x22);
    let pub2 = SpendPublic {
        anchor_root: same_block_root,
        nullifier: nf2,
        withdraw_amount: 0,
        output_commitments: vec![make_cm(&domain, 789, 0x23, 0x33)],
        view_attestations: None,
    };
    cache_pre_verified_spend(pub2.clone());

    let err_before_flush = mp
        .call(
            CallMessage::Transfer {
                proof: Default::default(),
                anchor_root: pub2.anchor_root,
                nullifier: pub2.nullifier,
                view_ciphertexts: None,
                gas: Some(<TestSpec as Spec>::Gas::zero()),
            },
            &ctx,
            &mut ws,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err_before_flush.contains("Invalid anchor root"),
        "expected InvalidAnchorRoot, got: {err_before_flush}"
    );

    // End-of-block: finalize the WorkingSet into a checkpoint and run end_block hook
    let (scratchpad, _, _) = ws.finalize();
    let mut end_cp = scratchpad.commit();

    // Inject foreign-height pending roots which must NOT be flushed by this block's hook.
    let other_height = end_cp.rollup_height_to_access().saturating_add(1);
    let foreign_root_1 = [0x42u8; 32];
    let foreign_root_2 = [0x43u8; 32];
    mp.pending_roots_indexed
        .set(
            &PendingRootKey {
                height: other_height.get(),
                idx: 0,
            },
            &foreign_root_1,
            &mut end_cp,
        )
        .unwrap();
    mp.pending_roots_indexed
        .set(
            &PendingRootKey {
                height: other_height.get(),
                idx: 1,
            },
            &foreign_root_2,
            &mut end_cp,
        )
        .unwrap();
    mp.pending_roots_count
        .set(&other_height, &2u32, &mut end_cp)
        .unwrap();

    mp.end_rollup_block_hook(&mut end_cp);

    // Current height's pending count is now 0; recent_roots contains initial + 2 new roots.
    let current_height_after_flush = end_cp.rollup_height_to_access();
    let pend_count_after_flush = mp
        .pending_roots_count
        .get(&current_height_after_flush, &mut end_cp)
        .unwrap()
        .unwrap_or(0);
    assert_eq!(pend_count_after_flush, 0);
    let recent_after_flush = mp.recent_roots.get(&mut end_cp).unwrap().unwrap();
    assert_eq!(recent_after_flush.len(), 3);
    assert_eq!(recent_after_flush.front().copied().unwrap(), initial_root);
    assert_eq!(recent_after_flush.back().copied().unwrap(), same_block_root);

    // Verify foreign-height count was NOT flushed or reset.
    let foreign_count_after_flush = mp
        .pending_roots_count
        .get(&other_height, &mut end_cp)
        .unwrap()
        .unwrap();
    assert_eq!(foreign_count_after_flush, 2);

    // Materialize the end-of-block changes to storage to start the next block
    {
        let (cache_log, accessory_delta, witness) = end_cp.freeze();
        let (new_root, change_set) =
            validate_and_materialize(storage, cache_log, &witness, sm.current_root()).unwrap();
        drop(accessory_delta);
        sm.commit_change_set(change_set, new_root);
    }

    // Block 2: same transfer anchored to same_block_root now succeeds.
    let storage2 = sm.create_storage();
    let cp2 =
        StateCheckpoint::<TestSpec>::new(storage2, &MockKernel::<TestSpec>::default());
    let scratchpad2 = cp2.to_tx_scratchpad();
    let tx2 = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
    let gas_meter2 = new_test_gas_meter::<TestSpec>();
    let mut ws2 = WorkingSet::<TestSpec>::create_working_set(scratchpad2, &tx2, gas_meter2);
    mp.call(
        CallMessage::Transfer {
            proof: Default::default(),
            anchor_root: pub2.anchor_root,
            nullifier: pub2.nullifier,
            view_ciphertexts: None,
            gas: Some(<TestSpec as Spec>::Gas::zero()),
        },
        &ctx,
        &mut ws2,
    )
    .unwrap();

    // And more roots got queued again for the current block.
    let current_height_block2 = ws2.rollup_height_to_access();
    let pend_count_after_t2 = mp
        .pending_roots_count
        .get(&current_height_block2, &mut ws2)
        .unwrap()
        .unwrap_or(0);
    assert_eq!(pend_count_after_t2, 1);

    // Attempting to anchor to a foreign-height root must still fail (not recorded in recent/all).
    let nf3: Hash32 = make_nf(&domain, 0x9B, 0x24);
    let pub3 = SpendPublic {
        anchor_root: foreign_root_1,
        nullifier: nf3,
        withdraw_amount: 0,
        output_commitments: vec![make_cm(&domain, 111, 0x24, 0x34)],
        view_attestations: None,
    };
    cache_pre_verified_spend(pub3.clone());
    let err_foreign_anchor = mp
        .call(
            CallMessage::Transfer {
                proof: Default::default(),
                anchor_root: pub3.anchor_root,
                nullifier: pub3.nullifier,
                view_ciphertexts: None,
                gas: Some(<TestSpec as Spec>::Gas::zero()),
            },
            &ctx,
            &mut ws2,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err_foreign_anchor.contains("Invalid anchor root"),
        "expected InvalidAnchorRoot for foreign-height root, got: {err_foreign_anchor}"
    );

    // Clean up cache
    clear_pre_verified_spend(&pub1.nullifier);
    clear_pre_verified_spend(&pub2.nullifier);
    clear_pre_verified_spend(&pub3.nullifier);
}


