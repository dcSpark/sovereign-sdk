#[cfg(all(feature = "maa", target_os = "linux"))]
use std::{env, fs, path::PathBuf, process::Command};

/// Path where the Azure Guest Attestation SDK installs the header (see attestation_verifier/README.md).
#[allow(dead_code)] // used only inside #[cfg(all(feature = "maa", target_os = "linux"))] block
const AZ_SDK_INCLUDE_HEADER: &str = "/usr/include/azguestattestation1/AttestationClient.h";

fn main() {
    // The MAA attestation client requires Azure-specific libraries (azguestattestation)
    // that are only available on Linux (and typically on Azure VMs). Skip building on non-Linux platforms.
    #[cfg(all(feature = "maa", target_os = "linux"))]
    {
        // Re-run if anything in the attestation_verifier directory changes.
        println!("cargo:rerun-if-changed=attestation_verifier");

        let sdk_header = PathBuf::from(AZ_SDK_INCLUDE_HEADER);
        if !sdk_header.exists() {
            println!(
                "cargo:warning=MAA attestation client not built: Azure Guest Attestation SDK not found (missing {}). Install it for real MAA attestation; local mock attestation (SOV_TEE_MOCK_ATTESTATION=1, ORACLE_DEV_ACCEPT_ALL=1) does not require it.",
                AZ_SDK_INCLUDE_HEADER
            );
            return;
        }

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

        println!("cargo:rustc-env=ATTESTATION_CLIENT_PATH={}", dest.display());
    }

    // On non-Linux platforms with maa feature, warn that attestation won't work
    #[cfg(all(feature = "maa", not(target_os = "linux")))]
    {
        println!("cargo:warning=MAA attestation client not built: requires Linux with Azure Guest Attestation library");
    }
}
