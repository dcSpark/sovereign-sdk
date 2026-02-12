//! Generates Nightstream artifacts for the value-setter-zk module.
//!
//! By default generates a full proof + transaction JSON + genesis config.
//! With --genesis-only, skips the expensive proof generation and only outputs
//! the genesis config and method_id (fast -- just computes the code commitment).
//!
//! Usage:
//!   cargo run -p sov-nightstream-adapter --example generate_proof_tx --features native -- [OPTIONS]
//!
//! Options:
//!   --value <N>            The value to prove and set (default: 42, ignored with --genesis-only)
//!   --output-dir <DIR>     Directory to write output files (default: .)
//!   --genesis-admin <ADDR> Admin address for genesis config (default: test key address)
//!   --genesis-only         Only generate genesis config + method_id (no proof, fast)

use std::io::Write;
use std::path::PathBuf;

use sha2::Digest;

// Include value-validator ROM from the Nightstream repo
mod value_validator_rom {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../Nightstream/crates/neo-fold/riscv-tests/binaries/value_validator_rom.rs"
    ));
}

fn main() -> anyhow::Result<()> {
    // Parse simple CLI args
    let args: Vec<String> = std::env::args().collect();
    let value = parse_arg(&args, "--value").unwrap_or(42u32);
    let output_dir = parse_arg_string(&args, "--output-dir").unwrap_or_else(|| ".".to_string());
    let genesis_admin = parse_arg_string(&args, "--genesis-admin")
        .unwrap_or_else(|| "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf".to_string());
    let genesis_only = args.iter().any(|a| a == "--genesis-only");

    let output_path = PathBuf::from(&output_dir);
    std::fs::create_dir_all(&output_path)?;

    if genesis_only {
        println!("=== Nightstream Genesis Config Generator ===");
    } else {
        println!("=== Nightstream Proof Transaction Generator ===");
        println!("Value to prove: {}", value);
    }
    println!("Output dir: {}", output_path.display());
    println!();

    // Load ROM
    let rom_bytes = &value_validator_rom::VALUE_VALIDATOR_ROM;
    let program_base = value_validator_rom::VALUE_VALIDATOR_ROM_BASE;

    println!(
        "ROM: {} bytes ({} instructions), base=0x{:x}",
        rom_bytes.len(),
        rom_bytes.len() / 4,
        program_base
    );

    // Compute code commitment (SHA-256 of ROM bytes)
    let method_id = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(rom_bytes);
        let result: [u8; 32] = hasher.finalize().into();
        result
    };

    println!("Code commitment (method_id): {}", hex::encode(method_id));
    println!();

    // ---- Write genesis config JSON ----
    println!("[1] Writing genesis config...");

    let method_id_array: Vec<u8> = method_id.to_vec();
    let genesis_json = serde_json::json!({
        "initial_value": 0,
        "method_id": method_id_array,
        "admin": genesis_admin,
        "backend": "nightstream"
    });

    let genesis_path = output_path.join("value_setter_zk.json");
    let mut genesis_file = std::fs::File::create(&genesis_path)?;
    serde_json::to_writer_pretty(&mut genesis_file, &genesis_json)?;
    genesis_file.flush()?;

    println!("  Genesis config written to: {}", genesis_path.display());

    // ---- Write method_id file ----
    println!("[2] Writing method_id...");

    let method_id_path = output_path.join("nightstream_method_id.hex");
    std::fs::write(&method_id_path, hex::encode(&method_id))?;

    println!("  Method ID written to: {}", method_id_path.display());

    // ---- genesis-only: stop here ----
    if genesis_only {
        println!();
        println!("=== Done (genesis-only) ===");
        println!("  Method ID: {}", hex::encode(method_id));
        println!("  Genesis:   {}", genesis_path.display());
        return Ok(());
    }

    // ---- Full mode: generate proof + transaction ----

    // Validate value range (guest enforces 0..=65535)
    if value > 65535 {
        anyhow::bail!("Value {} is out of range [0, 65535]", value);
    }

    println!("[3] Generating Nightstream proof...");

    use sov_nightstream_adapter::{NightstreamHost, NightstreamHostArgs};
    use sov_rollup_interface::zk::ZkvmHost;

    let args_host = NightstreamHostArgs::new(rom_bytes.to_vec(), program_base);
    let mut host = NightstreamHost::from_args(&args_host);

    // Configure inputs: proven=value, claimed=value
    host.add_u32_input(value);
    host.add_u32_input(value);

    // Output claim: guest writes validated value to RAM[0x100]
    host.add_output_claim(0x100, value as u64);

    let proof_bytes = host.run(true)?;

    println!(
        "  Proof generated successfully ({} bytes)",
        proof_bytes.len()
    );

    // ---- Write transaction JSON ----
    println!("[4] Writing transaction JSON...");

    // The CallMessage for value-setter-zk is:
    // { "set_value_with_proof": { "value": <u32>, "proof": [<u8 array>], "gas": null } }
    // SafeVec<u8, N> serializes as a plain JSON array of integers.
    let tx_json = serde_json::json!({
        "set_value_with_proof": {
            "value": value,
            "proof": proof_bytes,
            "gas": null
        }
    });

    let tx_path = output_path.join("nightstream_set_value_tx.json");
    let mut tx_file = std::fs::File::create(&tx_path)?;
    // Use compact JSON (no pretty-printing) to avoid massive whitespace bloat
    // on the multi-million-element proof byte array.
    serde_json::to_writer(&mut tx_file, &tx_json)?;
    tx_file.flush()?;

    println!("  Transaction JSON written to: {}", tx_path.display());

    // ---- Summary ----
    println!();
    println!("=== Summary ===");
    println!("  Value:       {}", value);
    println!("  Method ID:   {}", hex::encode(method_id));
    println!("  Proof size:  {} bytes", proof_bytes.len());
    println!("  TX JSON:     {}", tx_path.display());
    println!("  Genesis:     {}", genesis_path.display());
    println!();
    println!("Next steps:");
    println!("  1. Copy {} to the genesis config directory", genesis_path.display());
    println!("  2. Start the rollup with --nightstream feature");
    println!("  3. Use sov-cli to import and submit:");
    println!("     sov-cli transactions import from-file value-setter-zk \\");
    println!("       --chain-id 4321 --max-fee 10000000000 \\");
    println!("       --path {}", tx_path.display());
    println!("     sov-cli node submit-batch --wait-for-processing by-nickname <key>");

    Ok(())
}

fn parse_arg<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn parse_arg_string(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
