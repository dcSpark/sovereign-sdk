#![cfg(feature = "native")]

//! Tests to validate per-tx outbox parallelism safety.
//! These tests are based on the code paths in src/call.rs (queue_output behavior)
//! and prove that writes do not overlap and interleavings do not lose updates.

use midnight_privacy::{PendingOutput, ValueMidnightPrivacy};
use sov_modules_api::StateCheckpoint;
use sov_modules_api::capabilities::mocks::MockKernel;
use sov_rollup_interface::stf::ExecutionContext;
use sov_state::{Namespace, SlotKey, StateCodec, StateItemDecoder};
use sov_test_utils::storage::SimpleStorageManager;
use sov_test_utils::{TestSpec, TestStorageSpec};

fn cm(n: u8) -> [u8; 32] {
    [n; 32]
}

/// Test 1: No read/write-set conflict for per-tx queue_output
///
/// Creates two independent working sets on the same storage snapshot and
/// Writes to `pending_by_tx[txid]` in two separate tx contexts. Asserts no overlap.
#[test]
fn rw_sets_overlap_for_queue_output() {
    // Arrange: storage and two working sets over the same base
    let mut storage_mgr: SimpleStorageManager<TestStorageSpec> = SimpleStorageManager::new();
    let storage = storage_mgr.create_storage();
    let kernel = MockKernel::<TestSpec>::default();

    let checkpoint1 = StateCheckpoint::<TestSpec>::new(storage.clone(), &kernel);
    let checkpoint2 = StateCheckpoint::<TestSpec>::new(storage, &kernel);
    let mut ws1 = checkpoint1.to_working_set_unmetered();
    let mut ws2 = checkpoint2.to_working_set_unmetered();

    // Module accessors (state prefixes)
    let mut m1 = ValueMidnightPrivacy::<TestSpec>::default();
    let mut m2 = m1.clone();
    // New per-tx outbox keys
    let k_tx_map_prefix = m1.pending_by_tx.prefix().clone();

    // Act: TX1 writes to its own outbox (unique key)
    let txid1 = sov_rollup_interface::common::HexHash::new([7u8; 32]);
    {
        let mut outbox = m1
            .pending_by_tx
            .get(&txid1, &mut ws1)
            .unwrap()
            .unwrap_or_default();
        outbox.push(PendingOutput { cm: cm(1) });
        m1.pending_by_tx.set(&txid1, &outbox, &mut ws1).unwrap();
    }
    let (scratch1, _, _) = ws1.finalize();
    let changes1 = scratch1.tx_changes(ExecutionContext::Sequencer);

    // Act: TX2 writes to its own outbox (different key)
    let txid2 = sov_rollup_interface::common::HexHash::new([8u8; 32]);
    {
        let mut outbox = m2
            .pending_by_tx
            .get(&txid2, &mut ws2)
            .unwrap()
            .unwrap_or_default();
        outbox.push(PendingOutput { cm: cm(2) });
        m2.pending_by_tx.set(&txid2, &outbox, &mut ws2).unwrap();
    }
    let (scratch2, _, _) = ws2.finalize();
    let changes2 = scratch2.tx_changes(ExecutionContext::Sequencer);

    // Collect written user keys per tx
    let keys1: std::collections::BTreeSet<SlotKey> = changes1
        .writes
        .iter()
        .filter_map(|((k, ns), _)| if *ns == Namespace::User { Some(k.clone()) } else { None })
        .collect();
    let keys2: std::collections::BTreeSet<SlotKey> = changes2
        .writes
        .iter()
        .filter_map(|((k, ns), _)| if *ns == Namespace::User { Some(k.clone()) } else { None })
        .collect();

    // Expectation after fix: no overlap because writes go to distinct keys
    let overlap: Vec<_> = keys1.intersection(&keys2).cloned().collect();
    assert!(
        overlap.is_empty(),
        "expected per-tx isolation (no shared keys), found overlap: {:?}",
        overlap
    );
}

/// Test 2: Interleaving writes do not lose outputs under per-tx outboxes
///
/// A reads its outbox (empty), B writes to its own outbox, then A writes to its outbox using a stale
/// snapshot. Applying A after B (last-writer-wins) preserves both outboxes because keys are disjoint.
#[test]
fn last_writer_wins_loses_an_output() {
    // Arrange: two working sets over the same (empty) storage view
    let kernel = MockKernel::<TestSpec>::default();
    let storage = SimpleStorageManager::<TestStorageSpec>::new().create_storage();

    let checkpoint_a = StateCheckpoint::<TestSpec>::new(storage.clone(), &kernel);
    let checkpoint_b = StateCheckpoint::<TestSpec>::new(storage, &kernel);
    let mut ws_a = checkpoint_a.to_working_set_unmetered();
    let mut ws_b = checkpoint_b.to_working_set_unmetered();

    let mut m_a = ValueMidnightPrivacy::<TestSpec>::default();
    let mut m_b = m_a.clone();

    // Two distinct txids
    let txid_a = sov_rollup_interface::common::HexHash::new([1u8; 32]);
    let txid_b = sov_rollup_interface::common::HexHash::new([2u8; 32]);

    // A reads its own outbox (empty)
    let mut outbox_a = m_a
        .pending_by_tx
        .get(&txid_a, &mut ws_a)
        .unwrap()
        .unwrap_or_default();

    // B produces its changes in its own outbox
    {
        let mut outbox_b = m_b
            .pending_by_tx
            .get(&txid_b, &mut ws_b)
            .unwrap()
            .unwrap_or_default();
        outbox_b.push(PendingOutput { cm: cm(2) });
        m_b.pending_by_tx.set(&txid_b, &outbox_b, &mut ws_b).unwrap();
    }
    let (scratch_b, _, _) = ws_b.finalize();
    let changes_b = scratch_b.tx_changes(ExecutionContext::Sequencer);

    // A resumes (stale view), writes to its own outbox
    outbox_a.push(PendingOutput { cm: cm(1) });
    m_a.pending_by_tx.set(&txid_a, &outbox_a, &mut ws_a).unwrap();

    let (scratch_a, _, _) = ws_a.finalize();
    let changes_a = scratch_a.tx_changes(ExecutionContext::Sequencer);

    // Apply writes to an in-memory map in commit order: B then A (last-writer-wins)
    let mut db: std::collections::BTreeMap<(Namespace, SlotKey), Option<sov_state::SlotValue>> =
        Default::default();
    for ((k, ns), v) in changes_b.writes.iter() {
        db.insert((*ns, k.clone()), v.clone());
    }
    for ((k, ns), v) in changes_a.writes.iter() {
        db.insert((*ns, k.clone()), v.clone());
    }

    // Decode both outboxes: neither should be lost
    let key_a = m_a.pending_by_tx.slot_key(&txid_a);
    let key_b = m_a.pending_by_tx.slot_key(&txid_b);
    let codec = m_a.pending_by_tx.codec().value_codec();

    let outbox_a_sv = db
        .get(&(Namespace::User, key_a.clone()))
        .and_then(|opt| opt.clone())
        .expect("outbox A should be present");
    let outbox_b_sv = db
        .get(&(Namespace::User, key_b.clone()))
        .and_then(|opt| opt.clone())
        .expect("outbox B should be present");

    let outbox_a_dec: Vec<PendingOutput> = codec
        .try_decode(outbox_a_sv.value())
        .expect("decode outbox A");
    let outbox_b_dec: Vec<PendingOutput> = codec
        .try_decode(outbox_b_sv.value())
        .expect("decode outbox B");

    assert_eq!(outbox_a_dec.len(), 1, "A keeps its output");
    assert_eq!(outbox_b_dec.len(), 1, "B keeps its output");
}
