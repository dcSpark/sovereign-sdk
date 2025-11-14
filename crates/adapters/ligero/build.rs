use anyhow::Context;
use sov_zkvm_utils::should_skip_guest_build;
use std::path::{Path, PathBuf};
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
    let emcc_check = Command::new("emcc").arg("--version").output();

    if emcc_check.is_err() {
        println!("cargo:warning=Emscripten (emcc) not found. Ligero guest programs will not be built automatically.");
        println!(
            "cargo:warning=To build guest programs, install Emscripten and run './guest/build.sh'"
        );
        println!("cargo:warning=Install from: https://emscripten.org/docs/getting_started/downloads.html");
        return Ok(());
    }

    // Check if Ligero SDK is available
    let raw_sdk_path = if let Ok(env_path) = std::env::var("LIGERO_SDK_PATH") {
        PathBuf::from(env_path)
    } else {
        default_sdk_path()
    };

    let layout = resolve_sdk_layout(&raw_sdk_path).or_else(|| {
        if raw_sdk_path != default_sdk_path() {
            resolve_sdk_layout(&default_sdk_path())
        } else {
            None
        }
    });

    let (sdk_root, header_path) = match layout {
        Some(values) => values,
        None => {
            println!("cargo:warning=Ligero SDK headers and library not found. Searched in:");
            println!("cargo:warning=  {}", raw_sdk_path.display());
            if raw_sdk_path != default_sdk_path() {
                println!("cargo:warning=  {}", default_sdk_path().display());
            }
            println!(
                "cargo:warning=Skip building Ligero guests because SDK artifacts are missing."
            );
            println!("cargo:warning=Set LIGERO_SDK_PATH to the Ligero SDK root (containing 'build/libligetron.a').");
            return Ok(());
        }
    };

    println!(
        "cargo:warning=Resolved Ligero SDK root: {}",
        sdk_root.display()
    );
    println!(
        "cargo:warning=Using Ligero headers at: {}",
        header_path.display()
    );
    let lib_path = sdk_root.join("build/libligetron.a");
    println!(
        "cargo:warning=Using Ligero library at: {}",
        lib_path.display()
    );

    // Attempt to build guest programs if Emscripten and Ligero SDK are available
    let guest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest");
    let build_script = guest_dir.join("build.sh");

    if build_script.exists() {
        println!("cargo:warning=Building Ligero guest programs...");

        let ligero_sdk_path = sdk_root.to_string_lossy().to_string();
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

            // Show stdout
            if !build_result.stdout.is_empty() {
                let stdout = String::from_utf8_lossy(&build_result.stdout);
                for line in stdout.lines() {
                    println!("cargo:warning=  {}", line);
                }
            }

            // Show stderr
            if !build_result.stderr.is_empty() {
                let stderr = String::from_utf8_lossy(&build_result.stderr);
                for line in stderr.lines() {
                    println!("cargo:warning=  {}", line);
                }
            }
        } else {
            println!("cargo:warning=Ligero guest programs built successfully");
        }
    }

    Ok(())
}

fn default_sdk_path() -> PathBuf {
    // Get the workspace root (more reliable than relative paths)
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Navigate up to parent directory containing both repositories
    manifest_dir
        .parent() // -> crates/adapters
        .and_then(|p| p.parent()) // -> crates
        .and_then(|p| p.parent()) // -> sovereign-ligero root
        .and_then(|p| p.parent()) // -> parent directory
        .expect("Failed to find parent directory")
        .join("ligero-vm/ligero-prover/sdk")
}

fn resolve_sdk_layout(base: &Path) -> Option<(PathBuf, PathBuf)> {
    fn candidates(base: &Path) -> Vec<(PathBuf, PathBuf, PathBuf)> {
        let mut out = Vec::new();
        let root = base.to_path_buf();
        out.push((
            root.clone(),
            root.join("include/ligetron/api.h"),
            root.join("build/libligetron.a"),
        ));
        out.push((
            root.clone(),
            root.join("cpp/include/ligetron/api.h"),
            root.join("build/libligetron.a"),
        ));
        if let Some(parent) = base.parent() {
            let parent = parent.to_path_buf();
            out.push((
                parent.clone(),
                parent.join("include/ligetron/api.h"),
                parent.join("build/libligetron.a"),
            ));
            out.push((
                parent.clone(),
                base.join("include/ligetron/api.h"),
                parent.join("build/libligetron.a"),
            ));
        }
        out
    }

    for (root, header, lib) in candidates(base) {
        if header.exists() && lib.exists() {
            return Some((root, header));
        }
    }
    None
}
