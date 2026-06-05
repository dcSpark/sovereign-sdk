# Midnight Withdrawals Roadmap

This document tracks the work required to bring the Rust STF-side implementation of Midnight withdrawals in `demo-stf` (and, by extension, all rollups that embed it such as `rollup-ligero`) to parity with the Compact contracts shipped under `examples/rollup-ligero/midnight-l2-contracts`. Each milestone below produces a testable artifact so we can ship incremental value.

## Background

- The Compact contracts define the desired behavior: `L2Gateway` debits balances, `L2Messenger` hashes messages and leaves using helpers in `ProtocolTypes`, `L2MessageQueue` maintains a 16-level incremental tree, and L1 verifies proofs via `relayWithdrawNIGHTWithProof`.
- Our goal is to reproduce that logic on the STF side so the bridge background worker ( [examples/rollup-ligero/src/midnight_bridge.rs](../demo-rollup/src/midnight_bridge.rs) ) can originate real `relayWithdrawNIGHTWithProof` calls against the compiled contract artifacts (or on-chain deployment) without bespoke shims.

## Constraints

1. **Hashing / domain separation**: mirror the exact logic in `ProtocolTypes` (via the JS/WASM bundle in `examples/rollup-ligero/midnight-l2-contracts/bridge-contract/dist`). Do *not* switch to Poseidon; keep the SHA-256 based `persistentHash` domain separators already in the Compact sources.
2. **Tree depth**: keep the current 16-level queue so we match `WithdrawProof16`. Scaling to 40+ levels can be a later iteration once the Compact contract evolves.
3. **Module scope**: continue using a single `MidnightWithdrawals` module; only split it if a clean boundary emerges organically.
4. **TEE attestations**: skip wiring the withdraw root into batch attestations for now, but document the TODO and leave hook points.
5. **Location**: all STF changes ship through `examples/demo-rollup`, since `rollup-ligero` reuses that STF wholesale.

## Success Criteria

- We can generate the same withdrawal message hash and Merkle leaf as the Compact `L2Messenger` given identical inputs.
- The STF maintains the same append-only Merkle tree as `L2MessageQueue`; hashed siblings produced by the STF verify inside the Compact `Bridge` WASM via `relayWithdrawNIGHTWithProof`.
- The Midnight bridge background worker can assemble a proof by calling REST endpoints (or CLI helpers), submit it to the L1 contract, and observe `claimL1WithdrawalUnshielded` succeed in the managed WASM environment.

## Phased Plan

### Phase 1 — Protocol helpers & fixtures ✅

**Objective**: ensure Rust-side hashing matches Compact `ProtocolTypes`.

**Status (complete)**:
- `sov_midnight_adapter::protocol_types` ([crates/adapters/midnight/src/protocol_types.rs](../../../crates/adapters/midnight/src/protocol_types.rs)) implements `WithdrawMessage`, `hash_withdraw_message`, `hash_withdraw_leaf`, and `hash_merkle_node` with SHA-256 domain separators (`mdn:l2l1:wdraw`, `mdn:l2w:leaf`) matching Compact `ProtocolTypes`.
- Golden test vectors committed at `crates/adapters/midnight/test-data/protocol/golden-vectors-v1.json`, derived from the JS/WASM contract sources.
- Three unit tests (`withdrawal_hash_matches_golden_vector`, `withdraw_leaf_hash_matches_vector`, `zero_hash_levels_match_contract_reference`) confirm cross-language determinism: `cargo test -p sov-midnight-adapter protocol_types`.
- `MidnightWithdrawals` STF module imports these helpers and stores `message_hash` + `leaf_hash` in every `StoredWithdrawal` record.

### Phase 2 — In-module Merkle queue ✅

**Objective**: replace the ad-hoc `StateMap` with the incremental Merkle tree.

**Status (complete)**:
- `MidnightWithdrawals` now stores `message_count`, `withdraw_root`, and 16 cached branch values (`branch_0`–`branch_15`) as `StateValue` fields, mirroring `L2MessageQueue`.
- The append algorithm uses bit decomposition of the nonce, branch caching, and precomputed `ZERO_HASHES` (lazy-initialized). Helper functions `index_bits_le`, `zero_hash`, `zero_sibling`, and `compute_merkle_artifacts` implement the tree logic inline.
- REST endpoints expose the queue state (root, leaf count) and enriched per-withdrawal metadata including `message_hash` and `leaf_hash`.

### Phase 3 — Proof assembly API ✅

**Objective**: allow clients to fetch everything needed for `relayWithdrawNIGHTWithProof`.

**Status (complete)**:
- `GET /modules/midnight-withdrawals/withdrawals/{nonce}/proof?batch_index=<u64>` (requires `--features native`) returns the withdrawal payload, Merkle siblings, and an `l1_proof` blob matching Compact's `WithdrawProof16` layout. The `batch_index` query parameter lets bridge tooling stamp the finalized batch that attested the root.
- `L1WithdrawProof16Binary` struct encodes the proof with correct ordering and endianness for the Compact contract; `into_response()` serializes it as hex for the REST layer.
- Proof assembly recomputes the 16-level tree on demand via `compute_merkle_artifacts`, so the STF emits canonical siblings even after additional leaves are appended.
- Unit coverage: `cargo test -p demo-stf --features native merkle_artifacts_round_trip` verifies that proof data rebuilds the stored withdraw root.

### Phase 4 — Bridge worker integration ⬜

**Objective**: close the loop so proofs can be relayed automatically.

**Architecture**: the rollup already manages a TS executor service (`bridge-cli/src/executor-server.ts`) as a child process (see `examples/rollup-ligero/MIDNIGHT_BRIDGE.md`). The executor exposes `POST /relay-withdraw-night-with-proof` which delegates to `bridge-cli/src/commands/relay-withdraw-night-with-proof.ts` — this command calls `bridgeContract.callTx.relayWithdrawNIGHTWithProof(...)` via the Compact SDK and is ready to use. A Rust `executor_client::relay_withdraw_night_with_proof()` helper in `bridge-cli/decoder-rs/src/executor_client.rs` already wraps the HTTP call.

**Work items**:
- Add a withdrawal relay loop in `MidnightBridge` (`examples/rollup-ligero/src/midnight_bridge.rs`) that, after each finalized batch:
  1. Queries the STF REST API for unrelayed withdrawals and their proofs (`GET /modules/midnight-withdrawals/withdrawals/{nonce}/proof?batch_index=<u64>`).
  2. Calls the executor's `POST /relay-withdraw-night-with-proof` endpoint (via the existing `executor_client` helper or a direct HTTP POST) with the proof payload (`l2Sender`, `recipient`, `amount`, `batchIndex`, `nonce`, `indexBits`, `siblings`).
  3. Tracks which nonces have been successfully relayed (e.g., persisting a cursor in the accessory DB) to avoid duplicate submissions.
- Record the `withdraw_root` per finalized batch inside the STF state so the worker can confirm that the proof's root belongs to a published batch. (Full attestation wiring remains TODO; for now, store the root in a batch accessory DB or emit an event.)
- Optionally follow up with `POST /claim-l1-withdrawal-unshielded` (also available on the executor) so recipients can claim on L1 without a separate tool.
- Expand observability: metrics for queue length, proof generation latency, and relay success/failure counts.

**Testing**:
- End-to-end integration test using `bridge-cli/decoder-rs test-withdraw` (which already exercises the full deposit → withdraw → relay → claim flow against the executor) to validate the automated path.
- Manual runbook documented in `examples/rollup-ligero/MIDNIGHT_BRIDGE.md` for operators.

### Phase 5 — Hardening & TODOs ⬜

**Objective**: make the feature production-ready and highlight remaining gaps.

**Work items**:
- Add configuration flags for maximum queue depth warnings, proof generation limits, and REST pagination.
- Provide tooling to export/import queue state snapshots (useful for debugging or migration within devnets).
- Document outstanding TODOs, especially:
  - Wiring withdraw roots into `BatchPublicDataV1` and TEE attestations (blocked on the separate attestation effort).
  - Scaling the Merkle tree beyond 16 levels once the Compact contracts upgrade (`withdrawProof16` → flexible depth).
  - Switching to Poseidon-based hashing once the protocol mandates it.

**Testing**:
- Stress/regression tests covering queue persistence (drop/restart node), multiple concurrent withdrawals, and bridge worker restarts.
- Lint/docs checks to ensure all operator-facing docs reference the new flow.

## Next Steps

1. Phase 4 is next — wire the bridge worker to fetch proofs and relay `relayWithdrawNIGHTWithProof` calls to L1.
2. Track progress via a dedicated GitHub issue or project board so each phase can be merged independently.

---

**TODO (Attestation Integration Placeholder)**: once the TEE pipeline exposes `withdrawRoot` inside `BatchPublicDataV1`, we need to:
- Extend the STF batch builder to read the module's root after each slot and include it in the batch metadata.
- Update the bridge worker to verify that the proof's root matches the attested value instead of trusting local state.
- Ensure the L1 contract stores the root per batch so relays can reference finalized batch indices.

Until then, proofs should only be treated as devnet-quality evidence.
