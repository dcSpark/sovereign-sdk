//! Generate a Ligero proof for value-setter-zk module
//!
//! This tool generates a zero-knowledge proof that a value is within [0, 100]
//! using the Ligero zkVM and the value_validator.wasm guest program.
//!
//! Usage:
//!   cargo run --example generate_value_proof --features native -- <value>
//!
//! Example:
//!   cargo run --example generate_value_proof --features native -- 42

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sov_ligero_adapter::{Ligero, LigeroCodeCommitment, LigeroHost, LigeroProofPackage};
use sov_rollup_interface::zk::{CodeCommitment, Zkvm, ZkvmHost};

/// The public output structure that matches the guest program and value-setter-zk module
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValueProofPublic {
    value: u32,
}

fn main() -> Result<()> {
    // Parse command line arguments
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <value>", args[0]);
        eprintln!("  where <value> is a u32 in the range [0, 65535]");
        std::process::exit(1);
    }

    let value: u32 = args[1]
        .parse()
        .context("Failed to parse value as u32")?;

    if value > 65535 {
        anyhow::bail!("Value {} is out of range. Must be between 0 and 65535.", value);
    }

    println!("Generating Ligero proof for value: {}", value);
    println!("This may take a few moments...\n");

    // Find the value_validator.wasm program
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let program_path = PathBuf::from(manifest_dir)
        .join("guest/bins/programs/value_validator.wasm");

    if !program_path.exists() {
        anyhow::bail!(
            "value_validator.wasm not found at {:?}\n\
             Please compile the guest program first. See guest/README.md for instructions.",
            program_path
        );
    }

    println!("Using WASM program: {}", program_path.display());

    // Create Ligero host
    let mut host = <Ligero as Zkvm>::Host::from_args(
        &program_path.to_string_lossy().to_string()
    );

    // Configure with default packing
    host = host.with_packing(8192);

    // No private inputs for this example
    // (In a real scenario, you might mark certain inputs as private)
    
    // Add the proven value as the first argument (as raw bytes)
    let value_bytes = value.to_le_bytes();
    host.add_hex_arg(hex::encode(&value_bytes));
    
    // Add the claimed value as the second argument (same as proven value for generation)
    // During verification, this will be compared against the transaction's claimed value
    host.add_hex_arg(hex::encode(&value_bytes));

    // Get the code commitment (method ID)
    let commitment = host.code_commitment();
    println!("Code commitment: {}", hex::encode(commitment.encode()));

    // Generate the proof (this creates an incomplete package)
    println!("\nGenerating proof...");
    let _ = host.run(true)
        .context("Failed to generate proof")?;

    // Read the raw proof from proof.data
    let proof_file_path = PathBuf::from(manifest_dir).join("bins/proof.data");
    let raw_proof = std::fs::read(&proof_file_path)
        .context("Failed to read proof.data")?;

    println!("✓ Proof generated successfully!");
    println!("  Raw proof size: {} bytes", raw_proof.len());

    // Verify the proof locally
    println!("\nVerifying proof locally...");
    let verification_result = host.verify_proof()
        .context("Failed to verify proof")?;

    if !verification_result {
        anyhow::bail!("Proof verification failed!");
    }

    println!("✓ Proof verified successfully!");

    // Create the public output that matches what the guest program commits
    let public_output = ValueProofPublic { value };

    // Create the complete proof package
    let proof_package = LigeroProofPackage {
        proof: raw_proof,
        public_output,
    };

    // Serialize the proof package for submission
    let proof_data = bincode::serialize(&proof_package)
        .context("Failed to serialize proof package")?;

    // Output the proof in different formats
    println!("\n=== Proof Data ===");
    println!("Hex: {}", hex::encode(&proof_data));
    println!("\nJSON (for transaction):");
    println!("{}", serde_json::json!({
        "proof": hex::encode(&proof_data),
        "value": value,
        "commitment": hex::encode(commitment.encode()),
    }));

    // Save to file
    let proof_file = PathBuf::from("value_proof.bin");
    std::fs::write(&proof_file, &proof_data)
        .context("Failed to write proof to file")?;
    println!("\n✓ Proof saved to: {}", proof_file.display());

    // Create a transaction body template with proof as byte array
    let tx_template = serde_json::json!({
        "set_value_with_proof": {
            "value": value,
            "proof": proof_data,
            "gas": null
        }
    });

    let tx_file = PathBuf::from("value_tx.json");
    std::fs::write(&tx_file, serde_json::to_string_pretty(&tx_template)?)
        .context("Failed to write transaction template")?;
    println!("✓ Transaction template saved to: {}", tx_file.display());

    println!("\n=== Summary ===");
    println!("Value: {}", value);
    println!("Proof size: {} bytes", proof_data.len());
    println!("Code commitment: {}", hex::encode(commitment.encode()));
    println!("\nYou can now submit a transaction to the value-setter-zk module with this proof!");

    Ok(())
}

