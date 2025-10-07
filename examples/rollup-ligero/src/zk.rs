//! Small utilities for zk tooling

use std::sync::Arc;

/// Returns the ligero host arguments for a rollup with mock da (path to WASM program)
pub fn mock_da_ligero_host_args() -> Arc<String> {
    // Don't try to read the program if we're not building the ligero guest!
    if should_skip_guest_build() {
        return Arc::new(String::new());
    }

    // Return the path to the compiled Ligero guest program for mock DA
    Arc::new(ligero::MOCK_DA_PATH.to_string())
}

/// Returns the ligero host arguments for a rollup with celestia da (path to WASM program)
pub fn celestia_ligero_host_args() -> Arc<String> {
    if should_skip_guest_build() {
        return Arc::new(String::new());
    }

    // Return the path to the compiled Ligero guest program for Celestia
    Arc::new(ligero::ROLLUP_PATH.to_string())
}

fn should_skip_guest_build() -> bool {
    match std::env::var("SKIP_GUEST_BUILD")
        .as_ref()
        .map(|arg0: &String| String::as_str(arg0))
    {
        Ok("1") | Ok("true") | Ok("ligero") => true,
        Ok("0") | Ok("false") | Ok(_) | Err(_) => false,
    }
}

