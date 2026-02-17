use anyhow::{anyhow, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use midnight_privacy::Hash32;

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

pub fn decode_ligero_hash32_arg(v: &serde_json::Value, label: &str) -> Result<Hash32> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow!("Expected Ligero arg object for {label}"))?;

    if let Some(b64) = obj.get("bytes_b64").and_then(|v| v.as_str()) {
        let bytes = BASE64_STANDARD
            .decode(b64)
            .with_context(|| format!("Invalid base64 in {label}.bytes_b64"))?;
        let len = bytes.len();
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow!("{label}.bytes_b64 must decode to 32 bytes (got {len})"))?;
        return Ok(bytes);
    }

    let hex_str = obj
        .get("hex")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing {label}.hex"))?;
    let bytes = decode_hex_bytes(&format!("{label}.hex"), hex_str)?;
    let len = bytes.len();
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("{label}.hex must be 32 bytes (got {len})"))?;
    Ok(bytes)
}

pub fn inject_pool_sig_hex_into_proof_bytes(
    proof_bytes: Vec<u8>,
    _fvk_commitment_arg_pos: usize,
    _pool_sig_hex: String,
) -> Result<Vec<u8>> {
    // NightstreamProofPackage does not have args_json; pool sig injection is not supported.
    // Return proof bytes as-is. TODO: Add Goldilocks-based pool sig support for Nightstream.
    let _package: sov_nightstream_adapter::NightstreamProofPackage =
        bincode::deserialize(&proof_bytes).context("Proof payload is not a NightstreamProofPackage")?;
    Ok(proof_bytes)
}
