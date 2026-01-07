use std::sync::OnceLock;

/// Shared DB connection string for the storable Midnight DA database.
///
/// The `worker_verified_transactions` table lives in the same database as the storable DA
/// tables (`blobs`, `block_headers`, etc.). Some components (e.g. the sequencer worker-tx
/// endpoint and the Midnight Privacy pre-verified hydrator) need access to that DB but do not
/// have direct access to the DA config, so we stash the connection string here at startup.
static SHARED_DB_CONNECTION_STRING: OnceLock<String> = OnceLock::new();

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
pub fn shared_db_connection_string() -> Option<&'static str> {
    SHARED_DB_CONNECTION_STRING.get().map(|s| s.as_str())
}
