use sov_zkvm_utils::should_skip_guest_build;

fn main() {
    println!("cargo::rerun-if-env-changed=SKIP_GUEST_BUILD");
    println!("cargo::rerun-if-env-changed=OUT_DIR");

    if should_skip_guest_build("ligero") {
        println!("cargo:warning=Skipping Ligero guest build");
        let out_dir = std::env::var_os("OUT_DIR").unwrap();
        let out_dir = std::path::Path::new(&out_dir);
        let methods_path = out_dir.join("methods.rs");

        let wasm = r#"
            pub const ROLLUP_PATH: &str = "";
            pub const MOCK_DA_PATH: &str = "";
        "#;

        std::fs::write(methods_path, wasm).expect("Failed to write mock Ligero WASM paths");
    } else {
        // For Ligero, we need to compile the guest programs using Emscripten
        // and output their paths
        let out_dir = std::env::var_os("OUT_DIR").unwrap();
        let out_dir = std::path::Path::new(&out_dir);
        let methods_path = out_dir.join("methods.rs");

        // TODO: Actually compile the Ligero guest programs
        // For now, we'll just provide empty paths
        let wasm = r#"
            pub const ROLLUP_PATH: &str = "";
            pub const MOCK_DA_PATH: &str = "";
        "#;

        std::fs::write(methods_path, wasm).expect("Failed to write Ligero WASM paths");
    }
}
