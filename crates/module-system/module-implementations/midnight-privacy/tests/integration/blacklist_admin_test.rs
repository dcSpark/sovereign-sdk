#![cfg(feature = "native")]
#![allow(clippy::unwrap_used)]

use midnight_privacy::{
    cache_pre_verified_spend, clear_pre_verified_spend, default_blacklist_root, CallMessage,
    Hash32, MidnightPrivacyConfig, PrivacyAddress, SpendPublic, ValueMidnightPrivacy,
};
use sov_modules_api::capabilities::mocks::MockKernel;
use sov_modules_api::transaction::AuthenticatedTransactionData;
use sov_modules_api::{
    Context, Gas, Genesis, Module, Spec, StateCheckpoint, StateProvider, WorkingSet,
};
use sov_test_utils::storage::{ForklessStorageManager, SimpleStorageManager};
use sov_test_utils::{
    default_test_tx_details, new_test_gas_meter, validate_and_materialize, TestSpec,
    TestStorageSpec,
};

fn setup_mp() -> (
    ValueMidnightPrivacy<TestSpec>,
    SimpleStorageManager<TestStorageSpec>,
) {
    let mut sm = SimpleStorageManager::<TestStorageSpec>::new();
    sm.genesis();

    let mut mp = ValueMidnightPrivacy::<TestSpec>::default();

    let admin = <TestSpec as Spec>::Address::from([0xAA; 28]);
    let domain = [0x11; 32];
    let method_id = [0u8; 32];
    let token_id = sov_bank::TokenId::generate::<TestSpec>("NATIVE");

    let cfg = MidnightPrivacyConfig::<TestSpec> {
        tree_depth: 8,
        root_window_size: 16,
        method_id,
        admin,
        pool_admins: None,
        domain,
        token_id,
    };

    // Run module genesis against a checkpoint, then materialize to storage.
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

    (mp, sm)
}

fn new_ws(sm: &SimpleStorageManager<TestStorageSpec>) -> WorkingSet<TestSpec> {
    let storage = sm.create_storage();
    let cp = StateCheckpoint::<TestSpec>::new(storage, &MockKernel::<TestSpec>::default());
    let scratchpad = cp.to_tx_scratchpad();
    let tx = AuthenticatedTransactionData::<TestSpec>(default_test_tx_details::<TestSpec>());
    let gas_meter = new_test_gas_meter::<TestSpec>();
    WorkingSet::<TestSpec>::create_working_set(scratchpad, &tx, gas_meter)
}

fn ctx(sender: <TestSpec as Spec>::Address) -> Context<TestSpec> {
    let sequencer = <TestSpec as Spec>::Address::from([0xA2; 28]);
    let sequencer_da_addr: <<TestSpec as Spec>::Da as sov_modules_api::DaSpec>::Address =
        Default::default();
    Context::<TestSpec>::new(sender, Default::default(), sequencer, sequencer_da_addr)
}

#[test]
fn pool_admin_management_and_freeze_unfreeze() {
    let (mut mp, sm) = setup_mp();
    let mut ws = new_ws(&sm);

    let module_admin = <TestSpec as Spec>::Address::from([0xAA; 28]);
    let other = <TestSpec as Spec>::Address::from([0xBB; 28]);
    let new_pool_admin = <TestSpec as Spec>::Address::from([0xCC; 28]);

    // Non-admin cannot add pool admins.
    let err = mp
        .call(
            CallMessage::AddPoolAdmin {
                admin: new_pool_admin.clone(),
            },
            &ctx(other.clone()),
            &mut ws,
        )
        .unwrap_err();
    assert!(err.to_string().contains("Only module admin"));

    // Module admin can add a pool admin.
    mp.call(
        CallMessage::AddPoolAdmin {
            admin: new_pool_admin.clone(),
        },
        &ctx(module_admin.clone()),
        &mut ws,
    )
    .unwrap();

    // Pool admin can freeze/unfreeze an address (which updates blacklist_root).
    let pk: Hash32 = [0x11u8; 32];
    let addr = PrivacyAddress::from_pk(&pk);
    mp.call(
        CallMessage::FreezeAddress { address: addr },
        &ctx(new_pool_admin.clone()),
        &mut ws,
    )
    .unwrap();
    let frozen = mp.frozen_addresses.get(&mut ws).unwrap().unwrap_or_default();
    assert_eq!(frozen, vec![addr]);
    assert_ne!(
        mp.blacklist_root.get(&mut ws).unwrap().unwrap(),
        default_blacklist_root()
    );
    mp.call(
        CallMessage::UnfreezeAddress { address: addr },
        &ctx(new_pool_admin.clone()),
        &mut ws,
    )
    .unwrap();
    let frozen = mp.frozen_addresses.get(&mut ws).unwrap().unwrap_or_default();
    assert!(frozen.is_empty());
    assert_eq!(
        mp.blacklist_root.get(&mut ws).unwrap().unwrap(),
        default_blacklist_root()
    );

    // Module admin can remove a pool admin.
    mp.call(
        CallMessage::RemovePoolAdmin {
            admin: new_pool_admin.clone(),
        },
        &ctx(module_admin),
        &mut ws,
    )
    .unwrap();

    // Removed pool admin can no longer freeze/unfreeze.
    let err = mp
        .call(
            CallMessage::FreezeAddress { address: addr },
            &ctx(new_pool_admin),
            &mut ws,
        )
        .unwrap_err();
    assert!(err.to_string().contains("Only pool admins"));
}

#[test]
fn transfer_rejects_blacklist_root_mismatch() {
    let (mut mp, sm) = setup_mp();
    let mut ws = new_ws(&sm);

    let module_admin = <TestSpec as Spec>::Address::from([0xAA; 28]);

    // Freeze any address to move deny-map root away from the default.
    let pk: Hash32 = [0x22u8; 32];
    let addr = PrivacyAddress::from_pk(&pk);
    mp.call(
        CallMessage::FreezeAddress { address: addr },
        &ctx(module_admin.clone()),
        &mut ws,
    )
    .unwrap();

    // Build a pre-verified spend with the *default* root (mismatch).
    let anchor_root = mp.commitment_tree.get(&mut ws).unwrap().unwrap().root();
    let nullifier: Hash32 = [0x10u8; 32];
    let output: Hash32 = [0x22u8; 32];
    let public = SpendPublic {
        anchor_root,
        blacklist_root: default_blacklist_root(),
        nullifier,
        withdraw_amount: 0,
        output_commitments: vec![output],
        view_attestations: None,
    };
    cache_pre_verified_spend(public.clone());

    let sender = <TestSpec as Spec>::Address::from([0xA1; 28]);
    let err = mp
        .call(
            CallMessage::Transfer {
                proof: Default::default(),
                anchor_root: public.anchor_root,
                nullifier: public.nullifier,
                view_ciphertexts: None,
                gas: Some(<TestSpec as Spec>::Gas::zero()),
            },
            &ctx(sender),
            &mut ws,
        )
        .unwrap_err();
    assert!(err.to_string().contains("Blacklist root mismatch"));

    clear_pre_verified_spend(&public.nullifier);
}
