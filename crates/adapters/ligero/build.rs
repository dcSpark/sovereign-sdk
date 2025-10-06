use anyhow::Context;
use sov_zkvm_utils::should_skip_guest_build;
use std::path::PathBuf;
use std::process::Command;

/// Checks if Emscripten is available and the Ligero guest programs are compiled
fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-env-changed=SKIP_GUEST_BUILD");
    println!("cargo::rerun-if-env-changed=LIGERO_SDK_PATH");
    println!("cargo::rerun-if-changed=guest/");
    println!("cargo::rerun-if-changed=bins/programs/");

    // Skip the check if we aren't building any guest code
    if should_skip_guest_build("ligero") {
        println!("cargo:warning=Skipping Ligero guest build");
        return Ok(());
    }

    // Check if Emscripten is installed
    let emcc_check = Command::new("emcc")
        .arg("--version")
        .output();

    if emcc_check.is_err() {
        println!("cargo:warning=Emscripten (emcc) not found. Ligero guest programs will not be built automatically.");
        println!("cargo:warning=To build guest programs, install Emscripten and run './guest/build.sh'");
        println!("cargo:warning=Install from: https://emscripten.org/docs/getting_started/downloads.html");
        return Ok(());
    }

    // Check if Ligero SDK is available
    let ligero_sdk_path = std::env::var("LIGERO_SDK_PATH")
        .unwrap_or_else(|_| "../../../ligero-vm/ligero-prover/sdk".to_string());
    
    let sdk_path = PathBuf::from(&ligero_sdk_path);
    let lib_path = sdk_path.join("build/libligetron.a");

    if !lib_path.exists() {
        println!("cargo:warning=Ligero SDK library not found at: {}", lib_path.display());
        println!("cargo:warning=Please build the Ligero SDK first:");
        println!("cargo:warning=  cd {}", sdk_path.display());
        println!("cargo:warning=  mkdir -p build && cd build");
        println!("cargo:warning=  emcmake cmake ..");
        println!("cargo:warning=  emmake make -j");
        return Ok(());
    }

    // Attempt to build guest programs if Emscripten and Ligero SDK are available
    let guest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest");
    let build_script = guest_dir.join("build.sh");

    if build_script.exists() {
        println!("cargo:warning=Building Ligero guest programs...");
        
        let build_result = Command::new("bash")
            .arg(&build_script)
            .env("LIGERO_SDK_PATH", ligero_sdk_path)
            .current_dir(&guest_dir)
            .output()
            .context("Failed to execute guest build script")?;

        if !build_result.status.success() {
            println!("cargo:warning=Guest build failed. You may need to build manually:");
            println!("cargo:warning=  cd {}", guest_dir.display());
            println!("cargo:warning=  ./build.sh");
            if !build_result.stderr.is_empty() {
                println!("cargo:warning=Error: {}", String::from_utf8_lossy(&build_result.stderr));
            }
        } else {
            println!("cargo:warning=Ligero guest programs built successfully");
        }
    }

    Ok(())
}

