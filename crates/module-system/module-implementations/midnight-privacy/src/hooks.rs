//! Block hooks for the midnight-privacy module.

use sov_modules_api::{hooks::BlockHooks, Spec, StateCheckpoint};

use crate::ValueMidnightPrivacy;

impl<S: Spec> BlockHooks for ValueMidnightPrivacy<S> {
    type Spec = S;

    fn end_rollup_block_hook(&mut self, state: &mut StateCheckpoint<Self::Spec>) {
        // 1) Preferred path: apply from per-tx outboxes using block order
        if let Err(e) = self.apply_pending_inner(state) {
            tracing::error!("end_rollup_block_hook failed: {e:?}");
        }
    }
}
