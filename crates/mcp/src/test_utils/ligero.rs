//! Test utilities for Ligero proof generation

use std::path::PathBuf;

use crate::ligero::Ligero;

/// Helper function to get the platform-specific binary directory
#[allow(dead_code)]
pub fn get_platform_bin_dir() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos-arm64"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        panic!("Unsupported platform for Ligero tests");
    }
}

/// Helper function to create a Ligero instance for testing
#[allow(dead_code)]
pub fn create_test_ligero() -> Ligero {
    let platform_dir = get_platform_bin_dir();

    Ligero::new(
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!(
                    "../adapters/ligero/bins/{}/bin/webgpu_prover",
                    platform_dir
                ))
                .canonicalize()
                .expect("Failed to find prover binary"),
        ),
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../adapters/ligero/bins/shader")
                .canonicalize()
                .expect("Failed to find shader directory"),
        ),
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../adapters/ligero/guest/bins/programs/note_spend_guest.wasm")
                .canonicalize()
                .expect("Failed to find note_spend_guest.wasm"),
        ),
    )
}
