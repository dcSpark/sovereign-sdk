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
//! - sender_id: 32-byte sender identifier (only present in spend outputs, not deposits)

use clap::Parser;
use midnight_privacy::{
    viewing::{
        ct_hash as mp_ct_hash, fvk_commitment as mp_fvk_commitment, view_kdf as mp_view_kdf,
        view_mac as mp_view_mac,
    },
    FullViewingKey, Hash32, PrivacyAddress,
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
    /// Decrypted recipient as bech32 privacy address (derived from pk, if available)
    /// Note: This is the raw recipient hash, not the pk. For proper bech32 display,
    /// you need the original pk_out that was used to derive this recipient.
    recipient_hex: String,
    /// Decrypted sender_id (32 bytes hex) - present in spend outputs (144 bytes), absent in deposits (112 bytes)
    sender_id: Option<String>,
    /// Sender as bech32 privacy address (if sender_id present)
    sender_bech32: Option<String>,
    /// Commitments of notes spent to produce this tx (padded with zeros), when present.
    cm_ins: Option<Vec<String>>,
}

/// Convert a 32-byte hash to a bech32 privacy address string
fn hash_to_bech32(hash: &Hash32) -> String {
    PrivacyAddress::from_pk(hash).to_string()
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

/// Parse decrypted plaintext into note components.
///
/// Supports two formats:
/// - 112 bytes: Deposit notes [domain(32) | value(16) | rho(32) | recipient(32)]
/// - 144 bytes: Legacy spend outputs [domain(32) | value(16) | rho(32) | recipient(32) | sender_id(32)]
/// - 272 bytes: Spend outputs [domain(32) | value(16) | rho(32) | recipient(32) | sender_id(32) | cm_ins[4](128)]
fn parse_note_plaintext(
    pt: &[u8],
) -> Result<(Hash32, u128, Hash32, Hash32, Option<Hash32>, Option<Vec<Hash32>>), String> {
    if pt.len() != 112 && pt.len() != 144 && pt.len() != 272 {
        return Err(format!(
            "Expected 112, 144, or 272 bytes plaintext, got {}",
            pt.len()
        ));
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

    // Parse sender_id if present (spend outputs)
    let sender_id = if pt.len() == 144 || pt.len() == 272 {
        let mut sender = [0u8; 32];
        sender.copy_from_slice(&pt[112..144]);
        Some(sender)
    } else {
        None
    };

    let cm_ins = if pt.len() == 272 {
        let mut out = Vec::with_capacity(4);
        let mut off = 144usize;
        for _ in 0..4 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&pt[off..off + 32]);
            out.push(cm);
            off += 32;
        }
        Some(out)
    } else {
        None
    };

    Ok((domain, value, rho, recipient, sender_id, cm_ins))
}

fn decrypt_note(
    fvk: &Hash32,
    note: &EncryptedNoteInput,
    verify: bool,
) -> Result<DecryptedNote, String> {
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

    // Parse plaintext (supports 112-byte deposits and 144/272-byte spend outputs)
    let (domain, value, rho, recipient, sender_id, cm_ins) = parse_note_plaintext(&pt)?;

    // Convert to bech32 for display
    // Note: recipient is H(domain || pk), not the pk itself, so we display it as-is
    // The sender_id IS the spender's recipient (their address), so we can convert it to bech32
    let recipient_bech32 = hash_to_bech32(&recipient);
    let sender_bech32 = sender_id.map(|s| hash_to_bech32(&s));

    Ok(DecryptedNote {
        cm: note.cm.clone(),
        fvk_match,
        mac_valid,
        domain: hex::encode(domain),
        value,
        rho: hex::encode(rho),
        recipient: recipient_bech32,
        recipient_hex: hex::encode(recipient),
        sender_id: sender_id.map(|s| hex::encode(s)),
        sender_bech32,
        cm_ins: cm_ins.map(|arr| arr.into_iter().map(hex::encode).collect()),
    })
}

fn main() {
    let args = Args::parse();

    // Get FVK from CLI arg or environment variable
    let fvk_str = args
        .fvk
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
            io::stdin()
                .read_to_string(&mut buf)
                .expect("Failed to read stdin");
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
            io::stdin()
                .read_to_string(&mut buf)
                .expect("Failed to read stdin");
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
            println!(
                "cm,value,domain,rho,recipient,recipient_hex,sender,sender_hex,cm_ins,fvk_match,mac_valid"
            );
            for r in &results {
                println!(
                    "{},{},{},{},{},{},{},{},{},{},{}",
                    r.cm,
                    r.value,
                    r.domain,
                    r.rho,
                    r.recipient,
                    r.recipient_hex,
                    r.sender_bech32.as_deref().unwrap_or(""),
                    r.sender_id.as_deref().unwrap_or(""),
                    r.cm_ins
                        .as_ref()
                        .map(|v| v.join("|"))
                        .unwrap_or_default(),
                    r.fvk_match,
                    r.mac_valid
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "N/A".to_string())
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
                println!(
                    "  FVK Match:       {}",
                    if r.fvk_match { "✓ Yes" } else { "✗ No" }
                );
                if let Some(mac) = r.mac_valid {
                    println!("  MAC Valid:       {}", if mac { "✓ Yes" } else { "✗ No" });
                }
                println!("───────────────────────────────────────────────────────────────");
                println!("  Domain:          0x{}", r.domain);
                println!("  Value:           {} (0x{:032x})", r.value, r.value);
                println!("  Rho:             0x{}", r.rho);
                println!("  Recipient:       {}", r.recipient);
                println!("  Recipient (hex): 0x{}", r.recipient_hex);
                if let Some(ref sender) = r.sender_bech32 {
                    println!("  Sender:          {}", sender);
                }
                if let Some(ref sender_hex) = r.sender_id {
                    println!("  Sender (hex):    0x{}", sender_hex);
                }
                if let Some(ref cm_ins) = r.cm_ins {
                    println!("  cm_ins:");
                    for cm in cm_ins {
                        println!("    0x{}", cm);
                    }
                }
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

    /// Test decryption with a known encrypted note from a transfer (272 bytes, with sender_id).
    #[test]
    fn test_decrypt_known_transfer_note() {
        // Known FVK that was used to encrypt the note
        let fvk_hex = "897efbb9571eb74650851bff7ca467a51e30b1663de8e290de55900d53e00cab";
        let fvk = parse_hash32(fvk_hex).expect("valid FVK hex");

        // Encrypted note from a transfer transaction (272 bytes plaintext - includes sender_id and cm_ins)
        let note_json = r#"{
            "cm": "232d90124c95212b5614eba923b6caa41ab22de2d0edaa4d551e5dc0bb51158d",
            "nonce": "000000000000000000000000000000000000000000000000",
            "ct": [26,179,136,90,253,194,255,29,50,213,192,51,246,121,138,178,201,244,164,9,164,106,196,147,172,167,95,58,98,160,87,130,151,46,73,236,206,200,59,155,152,87,237,153,19,150,248,39,154,68,17,59,20,63,84,122,66,176,197,0,177,245,48,76,41,84,47,109,126,175,157,51,40,56,238,68,46,128,21,17,180,223,247,2,31,159,252,84,174,72,165,71,152,28,62,186,239,3,30,165,155,191,131,20,221,11,242,46,184,224,43,12,214,127,216,242,191,101,168,139,214,28,53,97,142,152,251,173,207,185,36,234,204,198,7,180,51,54,249,79,141,1,1,19,91,68,136,203,0,98,112,9,78,141,65,248,167,78,1,158,69,51,114,120,138,117,255,119,4,214,222,246,139,231,144,127,124,28,200,164,201,126,50,33,58,80,246,71,205,22,244,148,20,207,123,69,148,115,114,23,48,184,216,120,51,182,248,26,119,240,32,98,109,104,73,165,28,184,3,151,205,96,72,222,38,77,138,237,240,93,156,200,138,237,201,23,140,171,147,254,89,99,60,35,236,41,95,112,116,212,94,91,156,223,39,117,37,209,20,71,116,79,124,6,7,39,156,16,0,26,76,179],
            "fvk_commitment": "02b38c9c7f69ff932dc1f80b0c02ebe20e83facb3692020ca9f03ac9b78eca10",
            "mac": "2d4c1896e9931932a2771a366db25b7a448056bd682a51e8e95c3cd398a1d368"
        }"#;

        let note: EncryptedNoteInput = serde_json::from_str(note_json).expect("valid JSON");

        // Decrypt the note
        let decrypted = decrypt_note(&fvk, &note, true).expect("decryption should succeed");

        // Verify the decrypted values
        assert!(decrypted.fvk_match, "FVK commitment should match");
        assert_eq!(decrypted.mac_valid, Some(true), "MAC should be valid");

        // Transfer notes (144 bytes) HAVE sender_id
        assert!(
            decrypted.sender_id.is_some(),
            "Transfer notes should have sender_id"
        );
        assert!(
            decrypted.sender_bech32.is_some(),
            "Transfer notes should have sender_bech32"
        );

        // The note contains 150 tokens
        assert_eq!(decrypted.value, 150, "Value should be 150");

        // Domain should be the test domain (32 bytes of 0x01)
        assert_eq!(
            decrypted.domain, "0101010101010101010101010101010101010101010101010101010101010101",
            "Domain should be test domain"
        );

        // Verify rho
        assert_eq!(
            decrypted.rho, "738e8373832ee9cacb6f8f85b69822740b2b80adc48359ad8c4837084e5b6fe0",
            "Rho should match"
        );

        // Verify recipient (bech32)
        assert_eq!(
            decrypted.recipient,
            "privpool1rxvzkddu59n5rynf0wu54cfylr87jmv4rvgkqsfu36avua86zxwqveha7z",
            "Recipient bech32 should match"
        );
        assert_eq!(
            decrypted.recipient_hex,
            "19982b35bca1674192697bb94ae124f8cfe96d951b1160413c8ebace74fa119c",
            "Recipient hex should match"
        );

        // Verify sender (bech32)
        assert_eq!(
            decrypted.sender_bech32.as_deref(),
            Some("privpool1rxvzkddu59n5rynf0wu54cfylr87jmv4rvgkqsfu36avua86zxwqveha7z"),
            "Sender bech32 should match"
        );
        assert_eq!(
            decrypted.sender_id.as_deref(),
            Some("19982b35bca1674192697bb94ae124f8cfe96d951b1160413c8ebace74fa119c"),
            "Sender hex should match"
        );

        // Verify cm_ins are present
        assert!(
            decrypted.cm_ins.is_some(),
            "Spend notes should have cm_ins"
        );
        let cm_ins = decrypted.cm_ins.unwrap();
        assert_eq!(cm_ins.len(), 4, "Should have 4 cm_ins");
        assert_eq!(
            cm_ins[0], "10eff2ec48e83008e6e5c42eaf89dae06f3718744884df6e0875608e78f5e04f",
            "First cm_in should match"
        );
    }

    /// Test that decryption fails with wrong FVK
    #[test]
    fn test_decrypt_wrong_fvk_fails() {
        // Wrong FVK (different from the one used to encrypt)
        let wrong_fvk_hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let wrong_fvk = parse_hash32(wrong_fvk_hex).expect("valid FVK hex");

        // Use a real encrypted note but with wrong FVK
        let note_json = r#"{
            "cm": "1ae0a4df45de70e6e16be08b7cd832aff682c883923f447b7c5f33071b9e1370",
            "nonce": "000000000000000000000000000000000000000000000000",
            "ct": [9,77,2,17,54,208,121,54,222,205,62,35,86,219,165,244,172,104,231,12,248,223,124,164,209,75,25,18,110,109,200,51,22,225,191,218,180,90,201,231,185,114,22,203,213,158,113,206,84,141,145,114,5,100,136,87,92,210,14,55,55,159,95,35,159,91,205,115,243,209,166,158,44,211,112,205,64,60,50,132,245,112,222,1,198,34,192,36,116,34,255,190,25,221,245,251,235,253,229,20,158,144,145,88,42,119,214,169,236,196,20,202,197,222,18,222,55,18,46,128,215,28,1,47,129,21,17,227,224,2,149,26,170,218,203,152,186,199,79,192,45,224,192,244,49,30,81,182,118,122,232,140,9,94,133,141,214,54,235,226,96,74,197,105,120,87,122,81,249,28,202,164,219,112,51,226,105,150,133,235,229,200,59,90,45,231,13,223,120,20,7,4,18,187,81,58,199,135,35,142,173,36,135,237,89,19,115,106,224,105,11,59,230,134,134,108,40,91,231,148,119,247,127,115,45,31,231,15,245,125,72,9,164,84,72,240,204,248,151,91,217,217,74,243,37,77,39,32,97,39,8,118,92,106,221,76,8,251,232,81,98,129,184,229,130,0,70,98,14,120,192,46],
            "fvk_commitment": "02b38c9c7f69ff932dc1f80b0c02ebe20e83facb3692020ca9f03ac9b78eca10",
            "mac": "20951f5bf7ad28046d62ce0795f4621439b7d608d37530e869784d9f649be508"
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
        let fvk_hex = "897efbb9571eb74650851bff7ca467a51e30b1663de8e290de55900d53e00cab";
        let fvk = parse_hash32(fvk_hex).expect("valid FVK hex");

        let expected_commitment =
            "02b38c9c7f69ff932dc1f80b0c02ebe20e83facb3692020ca9f03ac9b78eca10";
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
