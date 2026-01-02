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
fn main() -> Result<()> {
    // Pass a circuit name (not a filesystem path). `ligero-runner` resolves the correct wasm.
    let program = std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string());

    println!("Computing method ID for: {}", program);

    let host = <Ligero as Zkvm>::Host::from_args(&program);
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

