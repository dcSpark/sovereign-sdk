use anyhow::Result;

/// Build script kept minimal now that guests are Rust-based.
fn main() -> Result<()> {
    println!("cargo::rerun-if-env-changed=SKIP_GUEST_BUILD");
    println!("cargo::rerun-if-changed=guest/");
    println!("cargo::rerun-if-changed=bins/programs/");
    Ok(())
}
