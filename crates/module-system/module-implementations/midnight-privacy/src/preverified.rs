use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::{Hash32, SpendPublic};

#[derive(Clone)]
struct CachedSpend {
    public: SpendPublic,
    remaining_consumers: u8,
}

type PreVerifiedMap = HashMap<Hash32, CachedSpend>;

static PRE_VERIFIED_SPENDS: OnceLock<Mutex<PreVerifiedMap>> = OnceLock::new();
static PRE_VERIFIED_MAP_ID: OnceLock<usize> = OnceLock::new();

// TODO: Implement a more sophisticated caching mechanism that allows for more efficient reuse of pre-verified spends.
// the module is called on both the sequencer and the node, so we need to ensure that the cached spends are not consumed too early.
const DEFAULT_CONSUMERS: u8 = 3;

fn map() -> &'static Mutex<PreVerifiedMap> {
    PRE_VERIFIED_SPENDS.get_or_init(|| {
        let mutex = Mutex::new(HashMap::new());
        let ptr = &mutex as *const _ as usize;
        let _ = PRE_VERIFIED_MAP_ID.set(ptr);
        mutex
    })
}

/// Caches pre-verified spend outputs keyed by their nullifier so the module can reuse them.
pub fn cache_pre_verified_spend(public: SpendPublic) {
    let mut guard = map().lock().unwrap();
    guard.insert(
        public.nullifier,
        CachedSpend {
            public: public.clone(),
            remaining_consumers: DEFAULT_CONSUMERS,
        },
    );
    tracing::debug!(
        target: "midnight_privacy::preverified",
        "[PRE-VERIFIED] cached pre-verified spend: {:?}, map_len: {}, map_id: {}",
        public,
        guard.len(),
        preverified_map_id()
    );
}

/// Retrieves (and consumes) a cached spend output for the provided nullifier, if any.
///
/// The cached entry is kept alive until it has been consumed `DEFAULT_CONSUMERS` times.
pub fn get_pre_verified_spend(nullifier: &Hash32) -> Option<SpendPublic> {
    let mut guard = map().lock().unwrap();
    let result = if let Some(entry) = guard.get_mut(nullifier) {
        let public = entry.public.clone();
        let remove = entry.remaining_consumers <= 1;
        if !remove {
            entry.remaining_consumers -= 1;
        }
        if remove {
            guard.remove(nullifier);
        }
        Some(public)
    } else {
        None
    };
    tracing::debug!(
        target: "midnight_privacy::preverified",
        "[PRE-VERIFIED] getting pre-verified spend for nullifier: {:?}, public: {:?}, map_len: {}, map_id: {}",
        nullifier,
        result,
        guard.len(),
        preverified_map_id()
    );
    result
}

/// Removes any cached spend output associated with the provided nullifier.
pub fn clear_pre_verified_spend(nullifier: &Hash32) {
    if let Some(lock) = PRE_VERIFIED_SPENDS.get() {
        let removed = lock.lock().unwrap().remove(nullifier);
        if removed.is_some() {
            tracing::debug!(
                target: "midnight_privacy::preverified",
                "[PRE-VERIFIED] cleared pre-verified spend: {:?}, map_id: {}",
                nullifier,
                preverified_map_id()
            );
        }
    }
}

/// Returns a stable identifier for the global pre-verified spend map.
fn preverified_map_id() -> usize {
    if let Some(id) = PRE_VERIFIED_MAP_ID.get() {
        *id
    } else {
        let ptr = map() as *const _ as usize;
        let _ = PRE_VERIFIED_MAP_ID.set(ptr);
        ptr
    }
}
