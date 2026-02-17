//! Generate an Authority Full Viewing Key (FVK) for Level-B compliance.
//!
//! This script generates a cryptographically secure 32-byte random key that can be used
//! as an authority viewing key. Authorities with this key can decrypt transaction
//! information (token amounts, sender, recipient) from the shielded pool.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk
//! ```
//!
//! Or with options:
//!
//! ```bash
//! # Save to file
//! cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk -- --output authority_fvk.txt
//!
//! # Generate multiple keys
//! cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk -- --count 3
//!
//! # Different output formats
//! cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk -- --format env
//! cargo run --release -p sov-rollup-nightstream --bin generate-authority-fvk -- --format json --count 3
//! ```
//!
//! # Environment Variable Usage
//!
//! The generated key can be used with e2e_runner or continuous-transfers by setting:
//!
//! ```bash
//! export AUTHORITY_FVK="0x<generated_key>"
//! ```

use clap::Parser;
use rand::RngCore;
use std::fs;
use std::path::PathBuf;

/// Generate Authority Full Viewing Keys for Level-B compliance
#[derive(Parser, Debug)]
#[command(name = "generate-authority-fvk")]
#[command(about = "Generate cryptographically secure Authority Full Viewing Keys (FVK)")]
#[command(long_about = r#"
Generate Authority Full Viewing Keys (FVK) for Level-B compliance.

An FVK is a 32-byte secret key that allows authorities to decrypt shielded
transaction data. The FVK commitment (hash of FVK) is public, but the FVK
itself must be kept secret.

CRYPTOGRAPHIC DETAILS:
  - FVK: 32 random bytes from a CSPRNG
  - FVK Commitment: Poseidon2("FVK_COMMIT_V1" || fvk)
  - Encryption Key: Poseidon2("VIEW_KDF_V1" || fvk || cm)

WHAT AUTHORITIES CAN SEE:
  - Token amounts in each output note
  - Random nonce (rho) - used for note uniqueness
  - Recipient binding - identifies the note owner

WHAT REMAINS HIDDEN:
  - Transaction graph (who transacts with whom)
  - Account balances (only individual notes visible)
  - Non-attested notes (from other provers)
"#)]
struct Args {
    /// Number of keys to generate
    #[arg(short, long, default_value = "1")]
    count: usize,

    /// Output file path (if not specified, prints to stdout)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output format: "hex" (default), "env" (export statement), or "json"
    #[arg(short, long, default_value = "hex")]
    format: String,

    /// Suppress informational messages (only output the key)
    #[arg(short, long)]
    quiet: bool,
}

fn generate_fvk() -> [u8; 32] {
    let mut fvk = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut fvk);
    fvk
}

fn format_fvk(fvk: &[u8; 32], format: &str, index: Option<usize>) -> String {
    let hex_str = hex::encode(fvk);
    let prefix = index.map(|i| format!("[{}] ", i + 1)).unwrap_or_default();

    match format {
        "env" => format!("{}export AUTHORITY_FVK=\"0x{}\"", prefix, hex_str),
        "json" => {
            if index.is_some() {
                format!("  \"0x{}\"", hex_str)
            } else {
                format!("\"0x{}\"", hex_str)
            }
        }
        _ => format!("{}0x{}", prefix, hex_str), // default: hex
    }
}

fn main() {
    let args = Args::parse();

    if args.count == 0 {
        eprintln!("Error: count must be at least 1");
        std::process::exit(1);
    }

    let mut output_lines = Vec::with_capacity(args.count);

    for i in 0..args.count {
        let fvk = generate_fvk();
        let index = if args.count > 1 { Some(i) } else { None };
        output_lines.push(format_fvk(&fvk, &args.format, index));
    }

    let output = if args.format == "json" && args.count > 1 {
        format!("[\n{}\n]", output_lines.join(",\n"))
    } else {
        output_lines.join("\n")
    };

    if let Some(path) = args.output {
        match fs::write(&path, format!("{}\n", output)) {
            Ok(_) => {
                if !args.quiet {
                    eprintln!(
                        "Generated {} FVK(s) and saved to: {}",
                        args.count,
                        path.display()
                    );
                }
            }
            Err(e) => {
                eprintln!("Error writing to file: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        println!("{}", output);

        if !args.quiet {
            eprintln!();
            eprintln!("─────────────────────────────────────────────────────────────");
            eprintln!("Authority Full Viewing Key(s) generated successfully!");
            eprintln!();
            eprintln!("To use with the e2e_runner, set the environment variable:");
            eprintln!("  export AUTHORITY_FVK=\"<key_from_above>\"");
            eprintln!();
            eprintln!("Or pass directly to the CLI:");
            eprintln!("  cargo run --bin e2e_runner_cli -- --authority-fvk \"<key>\"");
            eprintln!();
            eprintln!("Security notes:");
            eprintln!("  - Store this key securely - it allows decryption of transaction data");
            eprintln!("  - Share only with authorized compliance/audit entities");
            eprintln!("  - The FVK commitment (hash) is public; the FVK itself is secret");
            eprintln!("─────────────────────────────────────────────────────────────");
        }
    }
}
