use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    // Re-run if anything in the attestation_verifier directory changes.
    // Brutal but simple.
    println!("cargo:rerun-if-changed=attestation_verifier");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let att_dir = manifest.join("attestation_verifier");
    let bin_path = att_dir.join("AttestationClient");

    // Build every time build.rs runs (Cargo decides when that is).
    // This is simple. If it becomes too slow, then optimize later.
    let status = Command::new("cmake")
        .arg(".")
        .current_dir(&att_dir)
        .status()
        .unwrap();
    assert!(status.success(), "cmake failed");

    let status = Command::new("make")
        .arg("-C")
        .arg(&att_dir)
        .arg("AttestationClient")
        .status()
        .unwrap();
    assert!(status.success(), "make failed");

    // Copy into OUT_DIR
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("AttestationClient");
    fs::copy(&bin_path, &dest).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms).unwrap();
    }

    // ALWAYS emit, so env!/option_env! works.
    println!("cargo:rustc-env=ATTESTATION_CLIENT_PATH={}", dest.display());
}
