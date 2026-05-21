use crate::common::BatchPublicDataV1;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use borsh::{from_slice, to_vec};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    env,
    path::PathBuf,
    process::{Command, Stdio},
};

pub mod attestation;

const REQUIRED_TEE_MEASUREMENT_KEYS: [&str; 10] = [
    "measurements",
    "compliance-status",
    "attestation-type",
    "secureboot",
    "kerneldebug-enabled",
    "imageId",
    "launch_measurement",
    "microcode-svn",
    "snpfw-svn",
    "application_root_hash",
];


/// Function to attest the provided payload using the MAA service.
/// # Arguments
/// * `payload` - A struct containing the data to be attested.
/// * `nonce` - Attestation nonce
/// * `url` - Location of the server to send the attestation request to. If None, defaults to "http://attestation:8000/".
///
/// # Returns
/// * `Result<String, Error>` - The result of the attestation process.
pub async fn attest(payload: &BatchPublicDataV1, nonce: &str, url: Option<String>) -> Result<String> {
    // Serialize the payload to a byte array using borsh, needed for hashing.
    let payload = to_vec(&payload)?;

    let mut hasher = Sha256::new();

    // Hashing the payload
    // Domain separation tag
    hasher.update(b"midnight-l2::batch_data");
    hasher.update(&payload);
    let payload_hash = hasher.finalize();
    let payload_hash = format!("{:x}", payload_hash);

    println!("Payload hash: {}", payload_hash);

    let url = url.unwrap_or_else(|| "http://attestation:8000/".to_string());
    let response = reqwest::Client::new()
        .get(url + "?payload=" + &payload_hash + "&nonce=" + nonce)
        .header("Content-Type", "application/json")
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Attestation request failed with status {}: {}",
            response.status(),
            response.text().await.unwrap_or_else(|_| "Failed to read response body".to_string())
        );
    }
    let att = response.text().await?;
    Ok(att)
}


fn merge_tee_measurements(entries: [Value; 3]) -> Result<Value, anyhow::Error> {
    let mut merged = Map::new();

    for entry in entries {
        let Value::Object(object) = entry else {
            return Err(anyhow::anyhow!("each tee_measurements entry must be a JSON object"));
        };

        for (key, value) in object {
            if merged.insert(key.clone(), value).is_some() {
                return Err(anyhow::anyhow!("duplicate tee_measurements key: {}", key));
            }
        }
    }

    for key in REQUIRED_TEE_MEASUREMENT_KEYS {
        if !merged.contains_key(key) {
            return Err(anyhow::anyhow!("tee_measurements missing key: {}", key));
        }
    }

    if !merged
        .get("measurements")
        .is_some_and(serde_json::Value::is_object)
    {
        return Err(anyhow::anyhow!("tee_measurements.measurements must be a JSON object"));
    }

    if !merged
        .get("application_root_hash")
        .is_some_and(serde_json::Value::is_string)
    {
        return Err(anyhow::anyhow!("tee_measurements.application_root_hash must be a string"));
    }

    Ok(Value::Object(merged))
}

/// Function to verify the provided payload using the MAA service.
/// # Arguments
/// * `payload` - The JWT token to be verified.
/// * `tee_policy` - The attestation policy JSON to be used for verification.
/// * `root_hash` - The application root hash (application disk payload hash) JSON to be used for verification.
/// * `pcrs` - The PCRs hash JSON to be used for verification.
pub async fn verify(payload: &str, tee_policy: &str, root_hash: &str, pcrs: &str, dapp_url: &str) -> Result<(), anyhow::Error> {
    let pcrs = serde_json::from_str(pcrs)?;
    let root_hash = serde_json::from_str(root_hash)?;
    let tee_policy = serde_json::from_str(tee_policy)?;
    let attestation = serde_json::from_str(payload)?;
    let tee_measurements = merge_tee_measurements([pcrs, root_hash, tee_policy])?;
    attestation::verify_attestation_payload(attestation, dapp_url, &tee_measurements).await?;
    Ok(())
}