//! Circuit output parsing: raw Goldilocks output -> SpendPublic.
//!
//! The Nightstream note-spend circuit writes its public output as flat Goldilocks field elements:
//!
//! ```text
//! anchor(4xu64), n_in(u32), nullifiers[](4xu64 each), withdraw_amount(u64),
//! withdraw_to(4xu64), n_out(u32), output_cms[](4xu64 each), blacklist_root(4xu64)
//! ```
//!
//! This module provides `parse_circuit_output` to decode this layout into `SpendPublic`.

use anyhow::{anyhow, Result};
use p3_field::{PrimeCharacteristicRing, PrimeField64};
use p3_goldilocks::Goldilocks;

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
        return Err(anyhow!(
            "Not enough bytes for u32 at offset {}",
            offset
        ));
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&data[*offset..*offset + 4]);
    *offset += 4;
    Ok(u32::from_le_bytes(buf))
}

/// Read a u64 from a byte slice at a given offset (LE). Advances offset by 8.
fn read_u64(data: &[u8], offset: &mut usize) -> Result<u64> {
    if *offset + 8 > data.len() {
        return Err(anyhow!(
            "Not enough bytes for u64 at offset {}",
            offset
        ));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[*offset..*offset + 8]);
    *offset += 8;
    Ok(u64::from_le_bytes(buf))
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
}

/// Parse the raw circuit output bytes into a `CircuitOutput`.
///
/// Layout:
/// ```text
/// anchor(4xu64=32B), n_in(u32=4B), nullifiers[n_in](32B each),
/// withdraw_amount(u64=8B), withdraw_to(32B),
/// n_out(u32=4B), output_cms[n_out](32B each), blacklist_root(32B)
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

    Ok(CircuitOutput {
        anchor_root,
        nullifiers,
        withdraw_amount,
        withdraw_to,
        output_commitments,
        blacklist_root,
    })
}
