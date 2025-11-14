use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::{Hash32, SpendPublic};

type PreVerifiedMap = HashMap<Hash32, SpendPublic>;

static PRE_VERIFIED_SPENDS: OnceLock<Mutex<PreVerifiedMap>> = OnceLock::new();

fn map() -> &'static Mutex<PreVerifiedMap> {
    PRE_VERIFIED_SPENDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Caches pre-verified spend outputs keyed by their nullifier so the module can reuse them.
pub fn cache_pre_verified_spend(public: SpendPublic) {
    let mut guard = map().lock().unwrap();
    guard.insert(public.nullifier, public);
}

/// Retrieves a cached spend output for the provided nullifier, if any.
pub fn get_pre_verified_spend(nullifier: &Hash32) -> Option<SpendPublic> {
    map().lock().unwrap().get(nullifier).cloned()
}

/// Removes any cached spend output associated with the provided nullifier.
pub fn clear_pre_verified_spend(nullifier: &Hash32) {
    if let Some(lock) = PRE_VERIFIED_SPENDS.get() {
        lock.lock().unwrap().remove(nullifier);
    }
}
