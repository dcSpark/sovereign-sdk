use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey};
use midnight_privacy::{fvk_commitment, FullViewingKey, Hash32};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

use crate::pool_fvk::parse_hex_32;

const DEFAULT_FVK_SERVICE_URL: &str = "http://127.0.0.1:8088";

#[derive(Debug, Clone)]
pub struct ViewerFvkBundle {
    pub fvk: Hash32,
    pub fvk_commitment: Hash32,
    pub pool_sig_hex: String,
}

#[derive(Debug, Serialize)]
struct IssueFvkRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed_hex: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IssueFvkResponse {
    fvk: String,
    fvk_commitment: String,
    signature: String,
    signer_public_key: String,
    signature_scheme: String,
}

fn base_url_from_env() -> String {
    std::env::var("MIDNIGHT_FVK_SERVICE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_FVK_SERVICE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn parse_hex_64(label: &str, value: &str) -> Result<[u8; 64]> {
    let s = value.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).with_context(|| format!("Invalid hex for {label}"))?;
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_| anyhow!("{label} must be 64 bytes (got {len} bytes)"))
}

fn verify_commitment_signature(
    verifying_key: &VerifyingKey,
    fvk_commitment: &Hash32,
    signature: &[u8; 64],
) -> Result<()> {
    verifying_key
        .verify_strict(fvk_commitment, &Ed25519Signature::from_bytes(signature))
        .map_err(|e| anyhow!("Invalid pool signature over fvk_commitment: {e}"))
}

pub async fn fetch_viewer_fvk_bundle(
    http: &HttpClient,
    pool_fvk_pk: Option<[u8; 32]>,
) -> Result<ViewerFvkBundle> {
    let req = IssueFvkRequest {
        seed: None,
        seed_hex: None,
    };
    fetch_fvk_bundle_with_request(http, pool_fvk_pk, req).await
}

async fn fetch_fvk_bundle_with_request(
    http: &HttpClient,
    pool_fvk_pk: Option<[u8; 32]>,
    req: IssueFvkRequest,
) -> Result<ViewerFvkBundle> {
    let base_url = base_url_from_env();
    let endpoint = format!("{}/v1/fvk", base_url);

    let resp: IssueFvkResponse = http
        .post(&endpoint)
        .json(&req)
        .send()
        .await
        .with_context(|| format!("POST {endpoint}"))?
        .error_for_status()
        .with_context(|| format!("POST {endpoint} returned error status"))?
        .json()
        .await
        .context("Failed to deserialize midnight-fvk-service response")?;

    if resp.signature_scheme != "ed25519" {
        bail!(
            "midnight-fvk-service returned unsupported signature_scheme: {}",
            resp.signature_scheme
        );
    }

    let fvk = parse_hex_32("fvk", &resp.fvk)?;
    let fvk_commitment_resp = parse_hex_32("fvk_commitment", &resp.fvk_commitment)?;
    let signature = parse_hex_64("signature", &resp.signature)?;
    let signer_pk = parse_hex_32("signer_public_key", &resp.signer_public_key)?;

    let computed_commitment = fvk_commitment(&FullViewingKey(fvk));
    anyhow::ensure!(
        computed_commitment == fvk_commitment_resp,
        "midnight-fvk-service returned fvk_commitment that does not match fvk"
    );

    let signer_vk = VerifyingKey::from_bytes(&signer_pk)
        .map_err(|e| anyhow!("Invalid signer_public_key: {e}"))?;
    verify_commitment_signature(&signer_vk, &fvk_commitment_resp, &signature)?;

    if let Some(pool_pk) = pool_fvk_pk {
        anyhow::ensure!(
            pool_pk == signer_pk,
            "midnight-fvk-service signer_public_key does not match POOL_FVK_PK"
        );
        let pool_vk = VerifyingKey::from_bytes(&pool_pk)
            .map_err(|e| anyhow!("Invalid POOL_FVK_PK verifying key: {e}"))?;
        verify_commitment_signature(&pool_vk, &fvk_commitment_resp, &signature)?;
    }

    Ok(ViewerFvkBundle {
        fvk,
        fvk_commitment: fvk_commitment_resp,
        pool_sig_hex: resp.signature,
    })
}
