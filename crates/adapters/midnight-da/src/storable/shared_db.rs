use std::sync::OnceLock;

/// Shared DB connection string for the storable Midnight DA database.
///
/// The `worker_verified_transactions` table lives in the same database as the storable DA
/// tables (`blobs`, `block_headers`, etc.). Some components (e.g. the sequencer worker-tx
/// endpoint and the Midnight Privacy pre-verified hydrator) need access to that DB but do not
/// have direct access to the DA config, so we stash the connection string here at startup.
static SHARED_DB_CONNECTION_STRING: OnceLock<String> = OnceLock::new();

/// Cached value of the `SHARED_MIDNIGHT_DA_DB` environment variable (checked once).
/// Used as a fallback when no connection string was set programmatically (e.g. when
/// running with mock DA instead of the storable Midnight DA adapter).
static ENV_SHARED_DB_CONNECTION_STRING: OnceLock<Option<String>> = OnceLock::new();

/// Sets the shared storable Midnight DA DB connection string (best-effort).
///
/// If called multiple times with different values, the first value wins and a warning is logged.
pub fn set_shared_db_connection_string(connection_string: impl Into<String>) {
    let connection_string = connection_string.into();
    match SHARED_DB_CONNECTION_STRING.get() {
        Some(existing) => {
            if existing != &connection_string {
                tracing::warn!(
                    existing = %existing,
                    new = %connection_string,
                    "Attempted to set shared Midnight DA DB connection string twice with different values; keeping the first"
                );
            }
        }
        None => {
            let _ = SHARED_DB_CONNECTION_STRING.set(connection_string);
        }
    }
}

/// Returns the shared storable Midnight DA DB connection string, if configured.
///
/// Resolution order:
/// 1. Value set programmatically via [`set_shared_db_connection_string`].
/// 2. `SHARED_MIDNIGHT_DA_DB` environment variable (checked once, cached).
///
/// This fallback allows mock-DA rollups (which don't call
/// `set_shared_db_connection_string`) to participate in the worker-tx flow by
/// setting the env var in their launch script.
pub fn shared_db_connection_string() -> Option<&'static str> {
    // Prefer the programmatically-set value.
    if let Some(s) = SHARED_DB_CONNECTION_STRING.get() {
        return Some(s.as_str());
    }

    // Fallback: check env var (once).
    let env_val = ENV_SHARED_DB_CONNECTION_STRING.get_or_init(|| {
        match std::env::var("SHARED_MIDNIGHT_DA_DB") {
            Ok(val) if !val.is_empty() => {
                tracing::info!(
                    connection_string = %val,
                    "Using SHARED_MIDNIGHT_DA_DB env var as shared DB connection string"
                );
                Some(val)
            }
            _ => None,
        }
    });

    env_val.as_deref()
}
