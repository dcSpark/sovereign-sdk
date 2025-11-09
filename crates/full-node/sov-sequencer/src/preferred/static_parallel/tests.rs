use super::*;
use std::sync::{Arc, Mutex};

fn k(x: u8) -> StateKey {
    StateKey(vec![x])
}

#[test]
fn unknown_serializes() {
    let mut locks = KeyLocks::default();
    assert!(locks.try_lock(&AccessSet::UnknownExclusive));
    // Nothing else can be taken
    let a = AccessSet::Known {
        reads: Arc::from([k(1)]),
        writes: Arc::from([]),
    };
    assert!(!locks.try_lock(&a));
    locks.unlock(&AccessSet::UnknownExclusive);
    assert!(locks.try_lock(&a));
}

#[test]
fn readers_share_writers_exclude() {
    let mut locks = KeyLocks::default();
    let r1 = AccessSet::Known {
        reads: Arc::from([k(1)]),
        writes: Arc::from([]),
    };
    let r2 = AccessSet::Known {
        reads: Arc::from([k(1)]),
        writes: Arc::from([]),
    };
    let w = AccessSet::Known {
        reads: Arc::from([]),
        writes: Arc::from([k(1)]),
    };
    assert!(locks.try_lock(&r1));
    assert!(locks.try_lock(&r2));
    assert!(!locks.try_lock(&w)); // writer blocked by readers
    locks.unlock(&r1);
    locks.unlock(&r2);
    assert!(locks.try_lock(&w));
}

#[test]
fn writer_blocks_all_on_key() {
    let mut locks = KeyLocks::default();
    let w = AccessSet::Known {
        reads: Arc::from([]),
        writes: Arc::from([k(7)]),
    };
    let r = AccessSet::Known {
        reads: Arc::from([k(7)]),
        writes: Arc::from([]),
    };
    assert!(locks.try_lock(&w));
    assert!(!locks.try_lock(&r));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "AccessSet contains the same key in reads and writes")]
fn overlap_in_known_is_rejected_in_debug() {
    let mut locks = KeyLocks::default();
    let k1 = k(1);
    let a = AccessSet::Known {
        reads: Arc::from([k1.clone()]),
        writes: Arc::from([k1]),
    };
    // This should trigger debug assertion
    locks.try_lock(&a);
}

#[test]
fn duplicate_reads_and_writes_are_harmless() {
    let mut locks = KeyLocks::default();
    let k1 = k(1);
    let a = AccessSet::Known {
        reads: Arc::from([k1.clone(), k1.clone()]),
        writes: Arc::from([]),
    };
    assert!(locks.try_lock(&a));
    locks.unlock(&a); // refcounts go back to zero
    let w = AccessSet::Known {
        reads: Arc::from([]),
        writes: Arc::from([k1]),
    };
    assert!(locks.try_lock(&w)); // no leak
}

#[test]
fn stats_reflect_lock_state() {
    let mut locks = KeyLocks::default();
    assert_eq!(locks.stats(), (false, 0, 0));

    let r = AccessSet::Known {
        reads: Arc::from([k(1)]),
        writes: Arc::from([]),
    };
    let w = AccessSet::Known {
        reads: Arc::from([]),
        writes: Arc::from([k(2)]),
    };

    locks.try_lock(&r);
    assert_eq!(locks.stats(), (false, 1, 0));

    locks.try_lock(&w);
    assert_eq!(locks.stats(), (false, 1, 1));

    locks.unlock(&r);
    locks.unlock(&w);
    assert_eq!(locks.stats(), (false, 0, 0));

    locks.try_lock(&AccessSet::UnknownExclusive);
    assert_eq!(locks.stats(), (true, 0, 0));
}

#[test]
fn held_guard_unlocks_on_drop() {
    let locks = Arc::new(Mutex::new(KeyLocks::default()));
    let access = AccessSet::Known {
        reads: Arc::from([k(1)]),
        writes: Arc::from([]),
    };

    locks.lock().unwrap().try_lock(&access);
    assert_eq!(locks.lock().unwrap().stats(), (false, 1, 0));

    {
        let _held = Held::new(locks.clone(), access.clone());
        // held guard will unlock on drop
    }

    assert_eq!(locks.lock().unwrap().stats(), (false, 0, 0));
}

#[test]
fn access_set_known_from_with_dedup() {
    let k1 = k(1);
    let k2 = k(2);

    // With dedup - removes duplicates and read/write overlaps
    let access = AccessSet::known_from(vec![k1.clone(), k1.clone(), k2.clone()], vec![k2.clone()]);
    if let AccessSet::Known { reads, writes } = access {
        assert_eq!(reads.len(), 1); // k1 only, k2 filtered out as it's in writes
        assert_eq!(writes.len(), 1); // k2
        assert_eq!(reads[0], k1);
        assert_eq!(writes[0], k2);
    } else {
        panic!("Expected Known variant");
    }
}

#[test]
fn empty_known_becomes_unknown() {
    // Empty Known → Unknown for defensive serialization
    let a = AccessSet::from_sets(Arc::from([]), Arc::from([]));
    assert!(matches!(a, AccessSet::UnknownExclusive));

    // Also test via known_from
    let a2 = AccessSet::known_from(vec![], vec![]);
    assert!(matches!(a2, AccessSet::UnknownExclusive));
}

#[test]
#[cfg(not(debug_assertions))]  // This test intentionally violates debug assertion
fn duplicate_keys_dont_leak_locks() {
    // Verify that duplicate keys in reads/writes don't cause lock leaks
    // NOTE: In debug mode, overlapping reads/writes trigger an assertion.
    // This test verifies that IF it happens in release, it doesn't leak.
    let k1 = k(1);
    let mut locks = KeyLocks::default();
    
    // Lock with duplicates
    let a = AccessSet::Known {
        reads: Arc::from([k1.clone(), k1.clone()]),
        writes: Arc::from([k1.clone()]),
    };
    assert!(locks.try_lock(&a));
    locks.unlock(&a);
    
    // Should be able to lock again - no leak
    let b = AccessSet::Known {
        reads: Arc::from([k1]),
        writes: Arc::from([]),
    };
    assert!(locks.try_lock(&b));
}

#[test]
#[cfg(not(debug_assertions))]  // This test intentionally violates debug assertion
fn read_write_same_key_no_double_lock() {
    // If a key appears in both reads and writes, it should only be write-locked
    // NOTE: In debug mode, overlapping reads/writes trigger an assertion.
    // This test verifies the implementation handles it correctly if it happens.
    let k1 = k(1);
    let mut locks = KeyLocks::default();
    
    let a = AccessSet::Known {
        reads: Arc::from([k1.clone()]),
        writes: Arc::from([k1.clone()]),
    };
    
    // This should lock k1 as writer only (not also as reader)
    assert!(locks.try_lock(&a));
    
    // Verify k1 is in writers but not readers
    assert!(locks.writers.contains(&k1));
    assert!(!locks.readers.contains_key(&k1));
    
    locks.unlock(&a);
    assert_eq!(locks.stats(), (false, 0, 0));
}

