#!/usr/bin/env rust-script
//! Compute the method ID (code commitment) for note_spend_guest.wasm
//!
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! hex = "0.4"
//! sov-ligero-adapter = { path = "../../crates/adapters/ligero", features = ["native"] }
//! sov-rollup-interface = { path = "../../crates/rollup-interface" }
//! ```

use anyhow::{Context, Result};
use sov_ligero_adapter::Ligero;
use sov_rollup_interface::zk::{Zkvm, ZkvmHost, CodeCommitment};
use std::path::PathBuf;

fn main() -> Result<()> {
    // Find the note_spend_guest.wasm program
    let repo_root = std::env::current_dir()?
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .ok_or_else(|| anyhow::anyhow!("Could not find repository root"))?
        .to_path_buf();

    let program_path = repo_root
        .join("crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm");

    if !program_path.exists() {
        anyhow::bail!(
            "note_spend_guest.wasm not found at {}\n\
            Build it with: cd {} && ./build-guest-wasm.sh",
            program_path.display(),
            repo_root.join("crates/adapters/ligero/guest").display()
        );
    }

    println!("Computing method ID for: {}", program_path.display());

    let program_str = program_path.to_string_lossy().to_string();
    let host = <Ligero as Zkvm>::Host::from_args(&program_str);
    let method_id = host.code_commitment();
    
    let method_id_bytes = method_id.encode();
    let method_id_hex = hex::encode(method_id_bytes);

    println!("\n✓ Method ID: 0x{}", method_id_hex);
    println!("\nTo restart the proof verifier service with this method ID:");
    println!("\n  cd target/release");
    println!("  ./proof-verifier \\");
    println!("    --method-id 0x{} \\", method_id_hex);
    println!("    --bind 127.0.0.1:8080 \\");
    println!("    --node-rpc-url http://127.0.0.1:12346 \\");
    println!("    --signing-key-path ../../examples/test-data/keys/token_deployer_private_key.json \\");
    println!("    --chain-id 4321 \\");
    println!("    --max-concurrent 10\n");

    Ok(())
}

