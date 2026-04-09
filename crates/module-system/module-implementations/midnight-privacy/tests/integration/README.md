# Midnight Privacy Integration Tests

This directory contains integration tests for the `midnight-privacy` module.

## Test Files

### `blacklist_admin_test.rs`
Validates admin-controlled blacklist flows.

### `deferred_roots_test.rs`
Exercises deferred-root behavior and root transitions.

### `hash_tests.rs`
Checks hashing behavior and serialization consistency.

### `merkle_growth_tests.rs`
Verifies commitment tree growth and root evolution.

### `parallel_execution_conflict_test.rs`
Covers concurrent execution and conflict handling.

### `viewing_test.rs`
Exercises viewing-key and note visibility flows.

## Running the Tests

### Run all integration tests:
```bash
cargo test --test integration --features native
```

### Run a specific test:
```bash
cargo test --test integration viewing_test --features native -- --nocapture
```

### Run with verbose output:
```bash
cargo test --test integration --features native -- --nocapture --test-threads=1
```
