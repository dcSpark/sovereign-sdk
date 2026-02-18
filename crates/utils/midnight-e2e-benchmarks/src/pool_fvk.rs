use anyhow::{anyhow, Context, Result};

fn decode_hex_bytes(label: &str, s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).with_context(|| format!("Invalid hex for {label}"))
}

pub fn parse_hex_32(label: &str, value: &str) -> Result<[u8; 32]> {
    let bytes = decode_hex_bytes(label, value)?;
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_| anyhow!("{label} must be 32 bytes (got {len} bytes)"))
}

pub fn load_pool_fvk_pk_from_env() -> Result<Option<[u8; 32]>> {
    // If POOL_FVK_PK is explicitly set (even empty), it fully controls enforcement.
    if std::env::var_os("POOL_FVK_PK").is_some() {
        let pk = std::env::var("POOL_FVK_PK").unwrap_or_default();
        let pk = pk.trim();
        if pk.is_empty() {
            return Ok(None);
        }
        return Ok(Some(parse_hex_32("POOL_FVK_PK", pk)?));
    }

    let pk = std::env::var("MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX")
        .ok()
        .unwrap_or_default();
    let pk = pk.trim();
    if pk.is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_hex_32(
        "MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX",
        pk,
    )?))
}

/// If `POOL_FVK_PK` is not explicitly set but `MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX` is, export it
/// as `POOL_FVK_PK` so in-process verifier instances pick it up.
pub fn ensure_pool_fvk_pk_env() -> Result<Option<[u8; 32]>> {
    // Respect explicit POOL_FVK_PK even if empty (empty disables enforcement).
    if std::env::var_os("POOL_FVK_PK").is_some() {
        return load_pool_fvk_pk_from_env();
    }

    let fvk_service_pk = std::env::var("MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX")
        .ok()
        .unwrap_or_default();
    let fvk_service_pk = fvk_service_pk.trim();
    if !fvk_service_pk.is_empty() {
        std::env::set_var("POOL_FVK_PK", fvk_service_pk);
    }

    load_pool_fvk_pk_from_env()
}

pub fn inject_pool_sig_hex_into_proof_bytes(
    proof_bytes: Vec<u8>,
    _fvk_commitment_arg_pos: usize,
    pool_sig_hex: String,
) -> Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use sov_nightstream_adapter::{NightstreamProofPackage, PoolViewerSig};
    use std::io::{Read, Write};

    let sig_bytes = hex::decode(pool_sig_hex.trim())
        .context("pool_sig_hex is not valid hex")?;
    if sig_bytes.len() != 64 {
        anyhow::bail!(
            "pool_sig_hex must decode to 64 bytes (got {} bytes)",
            sig_bytes.len()
        );
    }

    let decompressed = {
        let mut decoder = DeflateDecoder::new(proof_bytes.as_slice());
        let mut buf = Vec::new();
        decoder
            .read_to_end(&mut buf)
            .context("Failed to decompress proof bytes")?;
        buf
    };

    let mut package: NightstreamProofPackage =
        bincode::deserialize(&decompressed).context("Failed to deserialize NightstreamProofPackage")?;

    let public: midnight_privacy::SpendPublic =
        bincode::deserialize(&package.public_output)
            .context("Failed to deserialize SpendPublic from package.public_output")?;

    let fvk_commitment = public
        .view_attestations
        .as_ref()
        .and_then(|atts| atts.first())
        .map(|att| att.fvk_commitment)
        .ok_or_else(|| anyhow!("Cannot inject pool sig: SpendPublic has no view_attestations"))?;

    package.pool_viewer_sig = Some(PoolViewerSig {
        fvk_commitment,
        signature: sig_bytes,
    });

    let raw = bincode::serialize(&package)
        .context("Failed to re-serialize NightstreamProofPackage")?;

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&raw)
        .context("Failed to write to deflate encoder")?;
    encoder
        .finish()
        .context("Failed to finish deflate compression")
}
