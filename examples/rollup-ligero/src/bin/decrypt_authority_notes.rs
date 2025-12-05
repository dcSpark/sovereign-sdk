//! Decrypt encrypted notes using an Authority Full Viewing Key (FVK).
//!
//! This script takes encrypted notes (from transaction events or worker DB) and
//! decrypts them using the authority's FVK, revealing the transaction details.
//!
//! # Usage
//!
//! ```bash
//! # From environment variable
//! export AUTHORITY_FVK="0x..."
//! cargo run --release -p sov-rollup-ligero --bin decrypt-authority-notes -- --input notes.json
//!
//! # From CLI argument
//! cargo run --release -p sov-rollup-ligero --bin decrypt-authority-notes -- \
//!   --fvk "0x..." \
//!   --input '[{"cm":"...","ct":[...],"fvk_commitment":"...","mac":"..."}]'
//!
//! # From stdin
//! echo '[{"cm":"...","ct":[...]}]' | cargo run -p sov-rollup-ligero --bin decrypt-authority-notes -- --fvk "0x..."
//! ```
//!
//! # Input Format
//!
//! JSON array of encrypted notes:
//! ```json
//! [{
//!   "cm": "hex string (32 bytes)",
//!   "nonce": "hex string (24 bytes, unused in Level-B)",
//!   "ct": [array of bytes] or "hex string",
//!   "fvk_commitment": "hex string (32 bytes)",
//!   "mac": "hex string (32 bytes)"
//! }]
//! ```
//!
//! # Output
//!
//! Decrypted note details including:
//! - domain: 32-byte domain identifier
//! - value: token amount (u128)
//! - rho: random nonce for note uniqueness
//! - recipient: 32-byte recipient identifier

use clap::Parser;
use midnight_privacy::{
    viewing::{fvk_commitment as mp_fvk_commitment, view_kdf as mp_view_kdf, ct_hash as mp_ct_hash, view_mac as mp_view_mac},
    FullViewingKey, Hash32,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

/// Decrypt encrypted notes using an Authority FVK
#[derive(Parser, Debug)]
#[command(name = "decrypt-authority-notes")]
#[command(about = "Decrypt Level-B encrypted notes using an Authority Full Viewing Key")]
struct Args {
    /// Authority FVK (32-byte hex, with or without 0x prefix).
    /// Can also be set via AUTHORITY_FVK environment variable.
    #[arg(long)]
    fvk: Option<String>,

    /// Input: file path, JSON string, or "-" for stdin
    #[arg(short, long)]
    input: Option<String>,

    /// Output format: "pretty" (default), "json", or "csv"
    #[arg(short, long, default_value = "pretty")]
    format: String,

    /// Verify MAC before decrypting (recommended)
    #[arg(long, default_value = "true")]
    verify_mac: bool,
}

/// Input encrypted note structure (flexible parsing)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct EncryptedNoteInput {
    cm: String,
    #[serde(default)]
    nonce: Option<String>, // Present in EncryptedNote but unused in Level-B
    ct: CtBytes,
    fvk_commitment: String,
    mac: String,
}

/// Flexible ciphertext parsing (array of bytes or hex string)
#[derive(Debug)]
struct CtBytes(Vec<u8>);

impl<'de> Deserialize<'de> for CtBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        
        // Try to deserialize as array first, then as hex string
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CtBytesHelper {
            Array(Vec<u8>),
            Hex(String),
        }
        
        match CtBytesHelper::deserialize(deserializer)? {
            CtBytesHelper::Array(arr) => Ok(CtBytes(arr)),
            CtBytesHelper::Hex(s) => {
                let s = s.strip_prefix("0x").unwrap_or(&s);
                hex::decode(s)
                    .map(CtBytes)
                    .map_err(|e| D::Error::custom(format!("Invalid hex in ct: {}", e)))
            }
        }
    }
}

/// Decrypted note output
#[derive(Debug, Serialize)]
struct DecryptedNote {
    /// Original note commitment
    cm: String,
    /// Whether FVK commitment matched
    fvk_match: bool,
    /// Whether MAC verified correctly
    mac_valid: Option<bool>,
    /// Decrypted domain (32 bytes hex)
    domain: String,
    /// Decrypted token value
    value: u128,
    /// Decrypted rho (32 bytes hex)
    rho: String,
    /// Decrypted recipient (32 bytes hex)
    recipient: String,
}

/// Parse a hex string to 32 bytes
fn parse_hash32(s: &str) -> Result<Hash32, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).map_err(|e| format!("Invalid hex: {}", e))?;
    if bytes.len() != 32 {
        return Err(format!("Expected 32 bytes, got {}", bytes.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Compute FVK commitment using midnight_privacy
fn fvk_commitment(fvk: &Hash32) -> Hash32 {
    mp_fvk_commitment(&FullViewingKey(*fvk))
}

/// Derive encryption key using midnight_privacy
fn view_kdf(fvk: &Hash32, cm: &Hash32) -> Hash32 {
    mp_view_kdf(&FullViewingKey(*fvk), cm)
}

/// Compute ciphertext hash using midnight_privacy
fn ct_hash(ct: &[u8]) -> Hash32 {
    mp_ct_hash(ct)
}

/// Compute MAC using midnight_privacy
fn view_mac(k: &Hash32, cm: &Hash32, ct_h: &Hash32) -> Hash32 {
    mp_view_mac(k, cm, ct_h)
}

/// Generate keystream block using Poseidon2
fn stream_block(k: &Hash32, ctr: u32) -> Hash32 {
    let c = ctr.to_le_bytes();
    midnight_privacy::poseidon2_hash(b"VIEW_STREAM_V1", &[k, &c])
}

/// Decrypt ciphertext using XOR with Poseidon2-based keystream
fn stream_xor_decrypt(k: &Hash32, ct: &[u8]) -> Vec<u8> {
    let mut pt = vec![0u8; ct.len()];
    let mut ctr = 0u32;
    let mut off = 0usize;
    
    while off < ct.len() {
        let ks = stream_block(k, ctr);
        ctr = ctr.wrapping_add(1);
        let take = core::cmp::min(32, ct.len() - off);
        for i in 0..take {
            pt[off + i] = ct[off + i] ^ ks[i];
        }
        off += take;
    }
    pt
}

/// Parse decrypted plaintext into note components
fn parse_note_plaintext(pt: &[u8]) -> Result<(Hash32, u128, Hash32, Hash32), String> {
    if pt.len() != 112 {
        return Err(format!("Expected 112 bytes plaintext, got {}", pt.len()));
    }
    
    let mut domain = [0u8; 32];
    domain.copy_from_slice(&pt[0..32]);
    
    let mut value_bytes = [0u8; 16];
    value_bytes.copy_from_slice(&pt[32..48]);
    let value = u128::from_le_bytes(value_bytes);
    
    let mut rho = [0u8; 32];
    rho.copy_from_slice(&pt[48..80]);
    
    let mut recipient = [0u8; 32];
    recipient.copy_from_slice(&pt[80..112]);
    
    Ok((domain, value, rho, recipient))
}

fn decrypt_note(fvk: &Hash32, note: &EncryptedNoteInput, verify: bool) -> Result<DecryptedNote, String> {
    let cm = parse_hash32(&note.cm)?;
    let expected_fvk_c = parse_hash32(&note.fvk_commitment)?;
    let expected_mac = parse_hash32(&note.mac)?;
    let ct = &note.ct.0;
    
    // Check FVK commitment matches
    let computed_fvk_c = fvk_commitment(fvk);
    let fvk_match = computed_fvk_c == expected_fvk_c;
    
    if !fvk_match {
        return Err(format!(
            "FVK commitment mismatch: expected {}, got {}. Wrong FVK?",
            note.fvk_commitment,
            hex::encode(computed_fvk_c)
        ));
    }
    
    // Derive decryption key
    let k = view_kdf(fvk, &cm);
    
    // Optionally verify MAC
    let mac_valid = if verify {
        let ct_h = ct_hash(ct);
        let computed_mac = view_mac(&k, &cm, &ct_h);
        let valid = computed_mac == expected_mac;
        if !valid {
            return Err(format!(
                "MAC verification failed: expected {}, got {}. Data may be tampered.",
                note.mac,
                hex::encode(computed_mac)
            ));
        }
        Some(true)
    } else {
        None
    };
    
    // Decrypt
    let pt = stream_xor_decrypt(&k, ct);
    
    // Parse plaintext
    let (domain, value, rho, recipient) = parse_note_plaintext(&pt)?;
    
    Ok(DecryptedNote {
        cm: note.cm.clone(),
        fvk_match,
        mac_valid,
        domain: hex::encode(domain),
        value,
        rho: hex::encode(rho),
        recipient: hex::encode(recipient),
    })
}

fn main() {
    let args = Args::parse();
    
    // Get FVK from CLI arg or environment variable
    let fvk_str = args.fvk
        .or_else(|| std::env::var("AUTHORITY_FVK").ok())
        .unwrap_or_else(|| {
            eprintln!("Error: FVK not provided. Set AUTHORITY_FVK env var or use --fvk");
            std::process::exit(1);
        });
    
    let fvk = match parse_hash32(&fvk_str) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error parsing FVK: {}", e);
            std::process::exit(1);
        }
    };
    
    // Get input
    let input_json = match &args.input {
        Some(input) if input == "-" => {
            // Read from stdin
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).expect("Failed to read stdin");
            buf
        }
        Some(input) if input.starts_with('[') || input.starts_with('{') => {
            // Treat as JSON string
            input.clone()
        }
        Some(path) => {
            // Treat as file path
            let path = PathBuf::from(path);
            fs::read_to_string(&path).unwrap_or_else(|e| {
                eprintln!("Error reading file {}: {}", path.display(), e);
                std::process::exit(1);
            })
        }
        None => {
            // Read from stdin
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).expect("Failed to read stdin");
            buf
        }
    };
    
    // Parse input (handle both single object and array)
    let notes: Vec<EncryptedNoteInput> = if input_json.trim().starts_with('[') {
        serde_json::from_str(&input_json).unwrap_or_else(|e| {
            eprintln!("Error parsing JSON array: {}", e);
            std::process::exit(1);
        })
    } else {
        let single: EncryptedNoteInput = serde_json::from_str(&input_json).unwrap_or_else(|e| {
            eprintln!("Error parsing JSON object: {}", e);
            std::process::exit(1);
        });
        vec![single]
    };
    
    if notes.is_empty() {
        eprintln!("No encrypted notes found in input");
        std::process::exit(1);
    }
    
    // Compute FVK commitment for display
    let fvk_c = fvk_commitment(&fvk);
    eprintln!("Using FVK commitment: 0x{}", hex::encode(fvk_c));
    eprintln!("Decrypting {} note(s)...\n", notes.len());
    
    // Decrypt each note
    let mut results: Vec<DecryptedNote> = Vec::new();
    let mut errors: Vec<(usize, String)> = Vec::new();
    
    for (i, note) in notes.iter().enumerate() {
        match decrypt_note(&fvk, note, args.verify_mac) {
            Ok(decrypted) => results.push(decrypted),
            Err(e) => errors.push((i, e)),
        }
    }
    
    // Output results
    match args.format.as_str() {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&results).unwrap());
        }
        "csv" => {
            println!("cm,value,domain,rho,recipient,fvk_match,mac_valid");
            for r in &results {
                println!(
                    "{},{},{},{},{},{},{}",
                    r.cm,
                    r.value,
                    r.domain,
                    r.rho,
                    r.recipient,
                    r.fvk_match,
                    r.mac_valid.map(|v| v.to_string()).unwrap_or_else(|| "N/A".to_string())
                );
            }
        }
        _ => {
            // Pretty format
            for (i, r) in results.iter().enumerate() {
                println!("═══════════════════════════════════════════════════════════════");
                println!("Note #{}", i + 1);
                println!("═══════════════════════════════════════════════════════════════");
                println!("  Commitment (cm): 0x{}", r.cm);
                println!("  FVK Match:       {}", if r.fvk_match { "✓ Yes" } else { "✗ No" });
                if let Some(mac) = r.mac_valid {
                    println!("  MAC Valid:       {}", if mac { "✓ Yes" } else { "✗ No" });
                }
                println!("───────────────────────────────────────────────────────────────");
                println!("  Domain:          0x{}", r.domain);
                println!("  Value:           {} (0x{:032x})", r.value, r.value);
                println!("  Rho:             0x{}", r.rho);
                println!("  Recipient:       0x{}", r.recipient);
                println!();
            }
        }
    }
    
    // Report errors
    if !errors.is_empty() {
        eprintln!("\n⚠️  {} note(s) failed to decrypt:", errors.len());
        for (i, e) in errors {
            eprintln!("  Note #{}: {}", i + 1, e);
        }
        std::process::exit(1);
    }
    
    eprintln!("✓ Successfully decrypted {} note(s)", results.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test decryption with a known encrypted note and FVK.
    /// This test uses real data from a transfer transaction.
    #[test]
    fn test_decrypt_known_note() {
        // Known FVK that was used to encrypt the note
        let fvk_hex = "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1";
        let fvk = parse_hash32(fvk_hex).expect("valid FVK hex");

        // Encrypted note from a real transaction
        let note_json = r#"{
            "cm": "525bd646059814e9c9155980fd953866a3276e2895eb0169c7d1f2fa4934fbac",
            "nonce": "000000000000000000000000000000000000000000000000",
            "ct": [2,27,141,24,12,246,151,115,74,188,182,154,109,8,141,128,234,9,26,66,102,68,121,109,167,169,208,190,252,248,211,212,242,248,44,61,11,135,93,229,227,119,48,250,239,199,15,181,80,72,49,3,167,197,214,113,15,237,169,120,141,251,166,117,200,161,38,58,32,25,21,125,102,158,41,212,145,128,160,71,33,162,18,70,69,243,34,67,222,67,199,120,164,206,128,218,229,200,211,230,158,86,98,44,136,203,160,171,22,210,9,19],
            "fvk_commitment": "10defb66061a9babdf3d75ae27129953ab6c05437e3370aaa40a3891ceb8c917",
            "mac": "6f4fb15e1241ba32b5357212d90c939ba7e45c1ada76bf684f26854c6ea61cab"
        }"#;

        let note: EncryptedNoteInput = serde_json::from_str(note_json).expect("valid JSON");

        // Decrypt the note
        let decrypted = decrypt_note(&fvk, &note, true).expect("decryption should succeed");

        // Verify the decrypted values
        assert!(decrypted.fvk_match, "FVK commitment should match");
        assert_eq!(decrypted.mac_valid, Some(true), "MAC should be valid");
        
        // The note contains 100 tokens
        assert_eq!(decrypted.value, 100, "Value should be 100");
        
        // Domain should be the test domain (32 bytes of 0x01)
        assert_eq!(
            decrypted.domain,
            "0101010101010101010101010101010101010101010101010101010101010101",
            "Domain should be test domain"
        );
        
        // Verify rho and recipient are correct
        assert_eq!(
            decrypted.rho,
            "d57b4a8476a9d04bd0e0ebcf531630e990aa3c89c042c23b53b15e18e6e3db17",
            "Rho should match"
        );
        assert_eq!(
            decrypted.recipient,
            "c5f030f6a93362dfde0a9705b9532cd87f26c4fde73f05fe6372dd5dc3172143",
            "Recipient should match"
        );
    }

    /// Test that decryption fails with wrong FVK
    #[test]
    fn test_decrypt_wrong_fvk_fails() {
        // Wrong FVK (different from the one used to encrypt)
        let wrong_fvk_hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let wrong_fvk = parse_hash32(wrong_fvk_hex).expect("valid FVK hex");

        let note_json = r#"{
            "cm": "525bd646059814e9c9155980fd953866a3276e2895eb0169c7d1f2fa4934fbac",
            "nonce": "000000000000000000000000000000000000000000000000",
            "ct": [2,27,141,24,12,246,151,115,74,188,182,154,109,8,141,128,234,9,26,66,102,68,121,109,167,169,208,190,252,248,211,212,242,248,44,61,11,135,93,229,227,119,48,250,239,199,15,181,80,72,49,3,167,197,214,113,15,237,169,120,141,251,166,117,200,161,38,58,32,25,21,125,102,158,41,212,145,128,160,71,33,162,18,70,69,243,34,67,222,67,199,120,164,206,128,218,229,200,211,230,158,86,98,44,136,203,160,171,22,210,9,19],
            "fvk_commitment": "10defb66061a9babdf3d75ae27129953ab6c05437e3370aaa40a3891ceb8c917",
            "mac": "6f4fb15e1241ba32b5357212d90c939ba7e45c1ada76bf684f26854c6ea61cab"
        }"#;

        let note: EncryptedNoteInput = serde_json::from_str(note_json).expect("valid JSON");

        // Decryption should fail because FVK commitment doesn't match
        let result = decrypt_note(&wrong_fvk, &note, true);
        assert!(result.is_err(), "Decryption with wrong FVK should fail");
        assert!(
            result.unwrap_err().contains("FVK commitment mismatch"),
            "Error should mention FVK commitment mismatch"
        );
    }

    /// Test FVK commitment computation matches expected value
    #[test]
    fn test_fvk_commitment_computation() {
        let fvk_hex = "fd3f0fc84254bcbe06977154d4db171a952201685f6ff8d5afe4a3c6e083f2b1";
        let fvk = parse_hash32(fvk_hex).expect("valid FVK hex");

        let expected_commitment = "10defb66061a9babdf3d75ae27129953ab6c05437e3370aaa40a3891ceb8c917";
        let computed_commitment = fvk_commitment(&fvk);

        assert_eq!(
            hex::encode(computed_commitment),
            expected_commitment,
            "FVK commitment should match expected value"
        );
    }

    /// Test parsing of encrypted note with ct as byte array
    #[test]
    fn test_parse_ct_as_array() {
        let json = r#"{"cm":"525bd646059814e9c9155980fd953866a3276e2895eb0169c7d1f2fa4934fbac","ct":[1,2,3],"fvk_commitment":"10defb66061a9babdf3d75ae27129953ab6c05437e3370aaa40a3891ceb8c917","mac":"6f4fb15e1241ba32b5357212d90c939ba7e45c1ada76bf684f26854c6ea61cab"}"#;
        let note: EncryptedNoteInput = serde_json::from_str(json).expect("should parse");
        assert_eq!(note.ct.0, vec![1u8, 2, 3]);
    }

    /// Test parsing of encrypted note with ct as hex string
    #[test]
    fn test_parse_ct_as_hex() {
        let json = r#"{"cm":"525bd646059814e9c9155980fd953866a3276e2895eb0169c7d1f2fa4934fbac","ct":"010203","fvk_commitment":"10defb66061a9babdf3d75ae27129953ab6c05437e3370aaa40a3891ceb8c917","mac":"6f4fb15e1241ba32b5357212d90c939ba7e45c1ada76bf684f26854c6ea61cab"}"#;
        let note: EncryptedNoteInput = serde_json::from_str(json).expect("should parse");
        assert_eq!(note.ct.0, vec![1u8, 2, 3]);
    }
}

