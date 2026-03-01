//! Circuit output parsing: raw Goldilocks output -> SpendPublic.
//!
//! The Nightstream note-spend circuit writes its public output as flat Goldilocks field elements:
//!
//! ```text
//! anchor(4xu64), n_in(u32), nullifiers[](4xu64 each), withdraw_amount(u64),
//! withdraw_to(4xu64), n_out(u32), output_cms[](4xu64 each), blacklist_root(4xu64),
//! n_viewers(u32), [per-viewer per-output: cm(4xu64), fvk_commitment(4xu64), ct_hash(4xu64), mac(4xu64)]
//! ```
//!
//! This module provides `parse_circuit_output` to decode this layout into `SpendPublic`.

use anyhow::{anyhow, Result};
use p3_field::{PrimeCharacteristicRing, PrimeField64};
use p3_goldilocks::Goldilocks;
use serde::{Deserialize, Serialize};

/// A 4-element Goldilocks digest (32 bytes as 4 x u64 LE).
pub type GlDigest = [Goldilocks; 4];

/// Convert a GlDigest to a 32-byte Hash32 (little-endian encoding).
pub fn gldigest_to_hash32(digest: &GlDigest) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, elem) in digest.iter().enumerate() {
        let val: u64 = elem.as_canonical_u64();
        out[i * 8..(i + 1) * 8].copy_from_slice(&val.to_le_bytes());
    }
    out
}

/// Read a GlDigest (4 x u64 LE) from a byte slice at a given offset.
/// Advances the offset by 32 bytes.
fn read_gldigest(data: &[u8], offset: &mut usize) -> Result<GlDigest> {
    if *offset + 32 > data.len() {
        return Err(anyhow!(
            "Not enough bytes for GlDigest at offset {}: need 32, have {}",
            offset,
            data.len() - *offset
        ));
    }
    let mut digest = [Goldilocks::ZERO; 4];
    for i in 0..4 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&data[*offset..*offset + 8]);
        digest[i] = Goldilocks::from_u64(u64::from_le_bytes(buf));
        *offset += 8;
    }
    Ok(digest)
}

/// Read a u32 from a byte slice at a given offset (LE). Advances offset by 4.
fn read_u32(data: &[u8], offset: &mut usize) -> Result<u32> {
    if *offset + 4 > data.len() {
        return Err(anyhow!("Not enough bytes for u32 at offset {}", offset));
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&data[*offset..*offset + 4]);
    *offset += 4;
    Ok(u32::from_le_bytes(buf))
}

/// Read a u64 from a byte slice at a given offset (LE). Advances offset by 8.
fn read_u64(data: &[u8], offset: &mut usize) -> Result<u64> {
    if *offset + 8 > data.len() {
        return Err(anyhow!("Not enough bytes for u64 at offset {}", offset));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[*offset..*offset + 8]);
    *offset += 8;
    Ok(u64::from_le_bytes(buf))
}

/// A single viewer attestation tuple parsed from the circuit output.
#[derive(Debug, Clone)]
pub struct CircuitViewAttestation {
    /// Output commitment this attestation is bound to.
    pub cm: [u8; 32],
    /// FVK commitment.
    pub fvk_commitment: [u8; 32],
    /// Ciphertext hash.
    pub ct_hash: [u8; 32],
    /// MAC.
    pub mac: [u8; 32],
}

/// Parsed circuit output before conversion to SpendPublic.
///
/// This is the raw data extracted from the circuit's public output.
/// The caller is responsible for converting to the module's `SpendPublic` type.
#[derive(Debug, Clone)]
pub struct CircuitOutput {
    /// Merkle tree anchor root.
    pub anchor_root: [u8; 32],
    /// Nullifiers (one per spent note).
    pub nullifiers: Vec<[u8; 32]>,
    /// Withdraw amount (u64 from circuit, zero-extend to u128 for SpendPublic).
    pub withdraw_amount: u64,
    /// Withdraw destination (from circuit; NOT part of SpendPublic).
    pub withdraw_to: [u8; 32],
    /// Output commitments.
    pub output_commitments: Vec<[u8; 32]>,
    /// Blacklist root.
    pub blacklist_root: [u8; 32],
    /// Viewer attestations (empty when n_viewers == 0).
    pub view_attestations: Vec<CircuitViewAttestation>,
}

/// Bincode/Serde-compatible view attestation shape used by `midnight_privacy::SpendPublic`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendPublicViewAttestation {
    /// Output commitment this attestation is bound to.
    pub cm: [u8; 32],
    /// FVK commitment.
    pub fvk_commitment: [u8; 32],
    /// Ciphertext hash.
    pub ct_hash: [u8; 32],
    /// MAC over `(k, cm, ct_hash)`.
    pub mac: [u8; 32],
}

/// Bincode/Serde-compatible SpendPublic shape used by `midnight_privacy`.
///
/// This keeps the adapter independent from the midnight-privacy crate while
/// still producing byte-identical bincode output for the same logical values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendPublicWire {
    /// Anchor root used for membership checks.
    pub anchor_root: [u8; 32],
    /// Deny-map root used for blacklist checks.
    pub blacklist_root: [u8; 32],
    /// Nullifiers of consumed notes.
    pub nullifiers: Vec<[u8; 32]>,
    /// Transparent withdrawal amount.
    pub withdraw_amount: u128,
    /// Commitments of newly created notes.
    pub output_commitments: Vec<[u8; 32]>,
    /// Optional viewer attestations.
    pub view_attestations: Option<Vec<SpendPublicViewAttestation>>,
}

/// Parse the raw circuit output bytes into a `CircuitOutput`.
///
/// Layout:
/// ```text
/// anchor(4xu64=32B), n_in(u32=4B), nullifiers[n_in](32B each),
/// withdraw_amount(u64=8B), withdraw_to(32B),
/// n_out(u32=4B), output_cms[n_out](32B each), blacklist_root(32B),
/// n_viewers(u32=4B), [per-viewer per-output: cm(32B), fvk_commitment(32B), ct_hash(32B), mac(32B)]
/// ```
pub fn parse_circuit_output(data: &[u8]) -> Result<CircuitOutput> {
    let mut offset = 0;

    let anchor_digest = read_gldigest(data, &mut offset)?;
    let anchor_root = gldigest_to_hash32(&anchor_digest);

    let n_in = read_u32(data, &mut offset)?;
    let mut nullifiers = Vec::with_capacity(n_in as usize);
    for _ in 0..n_in {
        let nf_digest = read_gldigest(data, &mut offset)?;
        nullifiers.push(gldigest_to_hash32(&nf_digest));
    }

    let withdraw_amount = read_u64(data, &mut offset)?;

    let withdraw_to_digest = read_gldigest(data, &mut offset)?;
    let withdraw_to = gldigest_to_hash32(&withdraw_to_digest);

    let n_out = read_u32(data, &mut offset)?;
    let mut output_commitments = Vec::with_capacity(n_out as usize);
    for _ in 0..n_out {
        let cm_digest = read_gldigest(data, &mut offset)?;
        output_commitments.push(gldigest_to_hash32(&cm_digest));
    }

    let bl_root_digest = read_gldigest(data, &mut offset)?;
    let blacklist_root = gldigest_to_hash32(&bl_root_digest);

    // Viewer attestations (may be absent if output was written by an older circuit)
    let mut view_attestations = Vec::new();
    if offset < data.len() {
        let n_viewers = read_u32(data, &mut offset)?;
        for _v in 0..n_viewers {
            for _j in 0..n_out {
                let cm = gldigest_to_hash32(&read_gldigest(data, &mut offset)?);
                let fvk_commitment = gldigest_to_hash32(&read_gldigest(data, &mut offset)?);
                let ct_hash = gldigest_to_hash32(&read_gldigest(data, &mut offset)?);
                let mac = gldigest_to_hash32(&read_gldigest(data, &mut offset)?);
                view_attestations.push(CircuitViewAttestation {
                    cm,
                    fvk_commitment,
                    ct_hash,
                    mac,
                });
            }
        }
    }

    if offset != data.len() {
        return Err(anyhow!(
            "Trailing bytes after parsing circuit output: offset={}, len={}",
            offset,
            data.len()
        ));
    }

    Ok(CircuitOutput {
        anchor_root,
        nullifiers,
        withdraw_amount,
        withdraw_to,
        output_commitments,
        blacklist_root,
        view_attestations,
    })
}

/// Convert output claims `(address, value)` into raw output bytes (sorted by address).
///
/// Each claim value must fit in `u32`, since the guest writes output as 32-bit words.
pub fn output_claims_to_bytes(output_claims: &[(u64, u64)]) -> Result<Vec<u8>> {
    let mut sorted: Vec<_> = output_claims.to_vec();
    sorted.sort_by_key(|&(addr, _)| addr);

    let mut bytes = Vec::with_capacity(sorted.len() * 4);
    for (addr, value) in sorted {
        let word = u32::try_from(value).map_err(|_| {
            anyhow!(
                "Output claim value does not fit in u32 at addr {:#x}: {}",
                addr,
                value
            )
        })?;
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    Ok(bytes)
}

/// Convert parsed circuit output into a bincode-compatible SpendPublic wire shape.
pub fn spend_public_wire_from_circuit_output(output: &CircuitOutput) -> SpendPublicWire {
    let view_attestations = if output.view_attestations.is_empty() {
        None
    } else {
        Some(
            output
                .view_attestations
                .iter()
                .map(|att| SpendPublicViewAttestation {
                    cm: att.cm,
                    fvk_commitment: att.fvk_commitment,
                    ct_hash: att.ct_hash,
                    mac: att.mac,
                })
                .collect(),
        )
    };

    SpendPublicWire {
        anchor_root: output.anchor_root,
        blacklist_root: output.blacklist_root,
        nullifiers: output.nullifiers.clone(),
        withdraw_amount: output.withdraw_amount as u128,
        output_commitments: output.output_commitments.clone(),
        view_attestations,
    }
}

/// Serialize parsed circuit output into bincode bytes compatible with SpendPublic.
pub fn spend_public_bytes_from_circuit_output(output: &CircuitOutput) -> Result<Vec<u8>> {
    let wire = spend_public_wire_from_circuit_output(output);
    bincode::serialize(&wire).map_err(|e| anyhow!("Failed to serialize SpendPublicWire: {}", e))
}

/// Derive certified SpendPublic bytes from output claims.
pub fn spend_public_bytes_from_output_claims(output_claims: &[(u64, u64)]) -> Result<Vec<u8>> {
    let raw = output_claims_to_bytes(output_claims)?;
    let parsed = parse_circuit_output(&raw)?;
    spend_public_bytes_from_circuit_output(&parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT_ADDR: u64 = 0x100;

    fn push_u32_claim(claims: &mut Vec<(u64, u64)>, addr: &mut u64, v: u32) {
        claims.push((*addr, v as u64));
        *addr += 4;
    }

    fn push_u64_claim(claims: &mut Vec<(u64, u64)>, addr: &mut u64, v: u64) {
        push_u32_claim(claims, addr, v as u32);
        push_u32_claim(claims, addr, (v >> 32) as u32);
    }

    fn push_digest_claim(claims: &mut Vec<(u64, u64)>, addr: &mut u64, d: &[u8; 32]) {
        for i in 0..4 {
            let mut word = [0u8; 8];
            word.copy_from_slice(&d[i * 8..(i + 1) * 8]);
            push_u64_claim(claims, addr, u64::from_le_bytes(word));
        }
    }

    #[test]
    fn output_claims_note_spend_to_spend_public_wire() {
        let anchor = [1u8; 32];
        let nullifier = [2u8; 32];
        let withdraw_to = [3u8; 32];
        let cm = [4u8; 32];
        let blacklist_root = [5u8; 32];

        let mut claims = Vec::new();
        let mut addr = OUTPUT_ADDR;
        push_digest_claim(&mut claims, &mut addr, &anchor);
        push_u32_claim(&mut claims, &mut addr, 1); // n_in
        push_digest_claim(&mut claims, &mut addr, &nullifier);
        push_u64_claim(&mut claims, &mut addr, 9); // withdraw_amount
        push_digest_claim(&mut claims, &mut addr, &withdraw_to);
        push_u32_claim(&mut claims, &mut addr, 1); // n_out
        push_digest_claim(&mut claims, &mut addr, &cm);
        push_digest_claim(&mut claims, &mut addr, &blacklist_root);
        push_u32_claim(&mut claims, &mut addr, 0); // n_viewers

        let bytes = spend_public_bytes_from_output_claims(&claims).expect("convert claims");
        let wire: SpendPublicWire = bincode::deserialize(&bytes).expect("deserialize wire");

        assert_eq!(wire.anchor_root, anchor);
        assert_eq!(wire.blacklist_root, blacklist_root);
        assert_eq!(wire.nullifiers, vec![nullifier]);
        assert_eq!(wire.withdraw_amount, 9);
        assert_eq!(wire.output_commitments, vec![cm]);
        assert!(wire.view_attestations.is_none());
    }

    #[test]
    fn parse_circuit_output_rejects_trailing_bytes() {
        let anchor = [7u8; 32];
        let withdraw_to = [8u8; 32];
        let blacklist_root = [9u8; 32];

        let mut claims = Vec::new();
        let mut addr = OUTPUT_ADDR;
        push_digest_claim(&mut claims, &mut addr, &anchor);
        push_u32_claim(&mut claims, &mut addr, 0); // n_in
        push_u64_claim(&mut claims, &mut addr, 0); // withdraw_amount
        push_digest_claim(&mut claims, &mut addr, &withdraw_to);
        push_u32_claim(&mut claims, &mut addr, 0); // n_out
        push_digest_claim(&mut claims, &mut addr, &blacklist_root);
        push_u32_claim(&mut claims, &mut addr, 0); // n_viewers

        let mut raw = output_claims_to_bytes(&claims).expect("claims bytes");
        raw.push(0xAA); // trailing byte

        let err = parse_circuit_output(&raw).expect_err("must reject trailing bytes");
        assert!(err.to_string().contains("Trailing bytes"));
    }
}
