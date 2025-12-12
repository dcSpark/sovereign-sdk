//! Parse SpendPublic from verified Ligero args.
//!
//! This module provides a secure way to derive `SpendPublic` from the args that
//! were actually verified by the Ligero proof, rather than trusting the unverified
//! `public_output` blob in `LigeroProofPackage`.
//!
//! # Security
//!
//! The Ligero verifier only binds `(program, packing, args, private_indices, proof)`.
//! The `public_output` field is NOT verified - it's just metadata. This parser
//! extracts the verified public inputs from args, closing the tampering vulnerability.

use crate::hash::Hash32;
use crate::types::{RecipientAttestation, SpendPublic, ViewAttestation};

/// Parse a 32-byte hex string (with optional 0x prefix) into a Hash32.
fn parse_hex32(s: &str) -> anyhow::Result<Hash32> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s)?;
    anyhow::ensure!(bytes.len() == 32, "expected 32-byte hex, got {} bytes", bytes.len());
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Extract string content from a LigeroArg.
fn arg_as_str(a: &sov_ligero_adapter::LigeroArg) -> anyhow::Result<&str> {
    use sov_ligero_adapter::LigeroArg::*;
    Ok(match a {
        String { str } => str.as_str(),
        Hex { hex } => hex.as_str(),
        I64 { .. } => anyhow::bail!("unexpected i64 arg in note_spend_guest ABI"),
    })
}

/// Parse a u64 from a LigeroArg (decimal string or i64).
fn arg_u64(a: &sov_ligero_adapter::LigeroArg) -> anyhow::Result<u64> {
    match a {
        sov_ligero_adapter::LigeroArg::I64 { i64: v } => {
            Ok(*v as u64)
        }
        sov_ligero_adapter::LigeroArg::String { str: s } => {
            Ok(s.trim().parse::<u64>()?)
        }
        sov_ligero_adapter::LigeroArg::Hex { hex: h } => {
            Ok(h.trim().parse::<u64>()?)
        }
    }
}

/// Parse a Hash32 from a LigeroArg (hex string).
fn arg_h32(a: &sov_ligero_adapter::LigeroArg) -> anyhow::Result<Hash32> {
    let s = arg_as_str(a)?;
    parse_hex32(s.trim())
}

/// Parse `SpendPublic` from verified Ligero args.
///
/// This function parses the note_spend_guest ABI to reconstruct `SpendPublic`
/// from the verified args. The args layout matches the guest program's ABI:
///
/// ```text
/// [1]  domain_hex       — 32-byte hex (PUBLIC)
/// [2]  spend_sk_hex     — 32-byte hex (PRIVATE/redacted)
/// [3]  depth_dec        — u64 decimal
/// [4]  anchor_hex       — 32-byte hex (PUBLIC)
/// [5]  n_in_dec         — u64 decimal in {1..=4}
///
/// For each input i in [0..n_in):
///   value_in_i_dec     — [PRIVATE/redacted]
///   rho_in_i_hex       — [PRIVATE/redacted]
///   pos_in_i_dec       — [PRIVATE/redacted]
///   siblings_i[k]_hex  — depth items [PRIVATE/redacted]
///   nullifier_i_hex    — 32-byte hex [PUBLIC]
///
/// Then:
///   withdraw_amount_dec — u64 decimal [PUBLIC]
///   n_out_dec           — u64 decimal in {0,1,2}
///
/// For each output j in [0..n_out):
///   value_out_j_dec      — [PRIVATE/redacted]
///   rho_out_j_hex        — [PRIVATE/redacted]
///   pk_spend_out_j_hex   — [PRIVATE/redacted]
///   pk_ivk_out_j_hex     — [PRIVATE/redacted]
///   cm_out_j_hex         — 32-byte hex [PUBLIC]
///   epk_out_j_hex        — 32-byte hex [PUBLIC]
///   ct_hash_out_j_hex    — 32-byte hex [PUBLIC]
///   mac_out_j_hex        — 32-byte hex [PUBLIC]
///
/// Optional viewer attestations:
///   n_viewers_dec      — u32 in {0..=8}
///   For each viewer:
///     fvk_commit_hex   — 32-byte hex [PUBLIC]
///     fvk_hex          — [PRIVATE/redacted]
///     For each output j:
///       ct_hash_j_hex  — 32-byte hex [PUBLIC]
///       mac_j_hex      — 32-byte hex [PUBLIC]
/// ```
pub fn spend_public_from_verified_args(
    args: &[sov_ligero_adapter::LigeroArg],
) -> anyhow::Result<SpendPublic> {
    let mut i = 0usize;

    // [1] domain (pub) - not used in SpendPublic, but part of ABI
    let _domain: Hash32 = arg_h32(&args[i])?;
    i += 1;

    // [2] spend_sk (priv/redacted) - skip
    i += 1;

    // [3] depth (pub)
    let depth = arg_u64(&args[i])? as usize;
    i += 1;

    // [4] anchor (pub)
    let anchor_root: Hash32 = arg_h32(&args[i])?;
    i += 1;

    // [5] n_in (pub)
    let n_in = arg_u64(&args[i])? as usize;
    i += 1;
    anyhow::ensure!((1..=4).contains(&n_in), "n_in out of range: {}", n_in);

    // Parse inputs
    let mut nullifiers: Vec<Hash32> = Vec::with_capacity(n_in);
    for _ in 0..n_in {
        // value (priv) - skip
        i += 1;
        // rho (priv) - skip
        i += 1;
        // pos (priv) - skip
        i += 1;
        // siblings[depth] (priv) - skip all
        i += depth;
        // nullifier (pub)
        let nf: Hash32 = arg_h32(&args[i])?;
        i += 1;
        nullifiers.push(nf);
    }

    // withdraw (pub)
    let withdraw_amount = arg_u64(&args[i])? as u128;
    i += 1;

    // n_out (pub)
    let n_out = arg_u64(&args[i])? as usize;
    i += 1;
    anyhow::ensure!(n_out <= 2, "n_out out of range: {}", n_out);

    // Parse outputs
    let mut output_commitments: Vec<Hash32> = Vec::with_capacity(n_out);
    let mut recipient_attestations: Option<Vec<RecipientAttestation>> = if n_out > 0 {
        Some(Vec::with_capacity(n_out))
    } else {
        None
    };

    for _ in 0..n_out {
        // value_out (priv) - skip
        i += 1;
        // rho_out (priv) - skip
        i += 1;
        // pk_spend_out (priv) - skip
        i += 1;
        // pk_ivk_out (priv) - skip
        i += 1;

        // cm_out (pub)
        let cm: Hash32 = arg_h32(&args[i])?;
        i += 1;
        // epk_out (pub)
        let epk: Hash32 = arg_h32(&args[i])?;
        i += 1;
        // ct_hash_out (pub)
        let ct_hash: Hash32 = arg_h32(&args[i])?;
        i += 1;
        // mac_out (pub)
        let mac: Hash32 = arg_h32(&args[i])?;
        i += 1;

        output_commitments.push(cm);

        if let Some(ref mut atts) = recipient_attestations {
            atts.push(RecipientAttestation {
                cm,
                epk,
                ct_hash,
                mac,
            });
        }
    }

    // Optional viewer attestations section
    let view_attestations = if i < args.len() {
        let n_viewers = arg_u64(&args[i])? as usize;
        i += 1;
        anyhow::ensure!(n_viewers <= 8, "n_viewers out of range: {}", n_viewers);

        if n_viewers == 0 {
            None
        } else {
            anyhow::ensure!(n_out > 0, "viewers not allowed when n_out == 0");
            let mut atts = Vec::with_capacity(n_viewers * n_out);

            for _ in 0..n_viewers {
                // fvk_commitment (pub)
                let fvk_commitment: Hash32 = arg_h32(&args[i])?;
                i += 1;
                // fvk (priv) - skip
                i += 1;

                for j in 0..n_out {
                    // ct_hash (pub)
                    let ct_hash: Hash32 = arg_h32(&args[i])?;
                    i += 1;
                    // mac (pub)
                    let mac: Hash32 = arg_h32(&args[i])?;
                    i += 1;

                    atts.push(ViewAttestation {
                        cm: output_commitments[j],
                        fvk_commitment,
                        ct_hash,
                        mac,
                    });
                }
            }
            Some(atts)
        }
    } else {
        None
    };

    // Verify we consumed all args (sanity check)
    anyhow::ensure!(
        i == args.len(),
        "unexpected trailing args: consumed {}, total {}",
        i,
        args.len()
    );

    Ok(SpendPublic {
        anchor_root,
        nullifiers,
        withdraw_amount,
        output_commitments,
        view_attestations,
        recipient_attestations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sov_ligero_adapter::LigeroArg;

    fn hex_arg(s: &str) -> LigeroArg {
        LigeroArg::String { str: s.to_string() }
    }

    fn dec_arg(v: u64) -> LigeroArg {
        LigeroArg::String { str: v.to_string() }
    }

    #[test]
    fn test_parse_minimal_transfer() {
        // Minimal: 1 input, 0 outputs, 0 withdraw (just nullifier reveal)
        // This won't pass circuit checks but tests parsing
        let zero32 = "0".repeat(64);

        let args = vec![
            hex_arg(&zero32),        // domain
            hex_arg(&zero32),        // spend_sk (redacted)
            dec_arg(20),             // depth
            hex_arg("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"), // anchor
            dec_arg(1),              // n_in
            // Input 0:
            dec_arg(0),              // value (redacted)
            hex_arg(&zero32),        // rho (redacted)
            dec_arg(0),              // pos (redacted)
            // 20 siblings (all redacted)
            hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32),
            hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32),
            hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32),
            hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32),
            hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32), hex_arg(&zero32),
            hex_arg("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), // nullifier
            dec_arg(0),              // withdraw
            dec_arg(0),              // n_out
        ];

        let public = spend_public_from_verified_args(&args).unwrap();
        
        assert_eq!(
            hex::encode(public.anchor_root),
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"
        );
        assert_eq!(public.nullifiers.len(), 1);
        assert_eq!(
            hex::encode(public.nullifiers[0]),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(public.withdraw_amount, 0);
        assert!(public.output_commitments.is_empty());
        assert!(public.view_attestations.is_none());
        assert!(public.recipient_attestations.is_none());
    }
}
