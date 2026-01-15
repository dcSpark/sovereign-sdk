use crate::common::BatchPublicDataV1;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use borsh::{from_slice, to_vec};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    path::PathBuf,
    process::{Command, Stdio},
};

/// Function to decode the JWT payload into a JSON value.
/// The verification of the JWT signature is not done here, as it is done by the AttestationClient.
pub fn jwt_payload_json(jwt: &str) -> Result<Value> {
    let payload_b64 = jwt.split('.').nth(1).context("JWT missing payload")?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("payload is not valid base64url")?;
    let v: Value = serde_json::from_slice(&payload_bytes).context("payload is not valid JSON")?;
    Ok(v)
}

/// Function to get the path to the AttestationClient executable.
pub fn attestation_client_path() -> Result<PathBuf> {
    // Mostly useful in CI tests or with Cargo test.
    if let Some(p) = option_env!("ATTESTATION_CLIENT_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
    }

    // If the value ATTESTATION_CLIENT_PATH is not set, we assume the AttestationClient is in the same directory as the current executable.
    let exe = env::current_exe().context("cannot get current executable path")?;

    let client = exe
        .parent()
        .context("executable has no parent directory")?
        .join("AttestationClient");

    if !client.exists() {
        anyhow::bail!("AttestationClient not found");
    }

    Ok(client)
}

/// Function to attest the provided payload using the MAA service.
/// # Arguments
/// * `payload` - A struct containing the data to be attested.
///
/// # Returns
/// * `Result<String, Error>` - The result of the attestation process.
pub fn attest(payload: &BatchPublicDataV1, nonce: &str) -> Result<String> {
    // As MAA requires a C++ library, we call an external C++ program to handle the attestation process.
    // Easier and less time consuming than writing bindings...

    // Get the path to the AttestationClient executable.
    let client = attestation_client_path()?;

    // Serialize the payload to a byte array using borsh, needed for hashing.
    let payload = to_vec(&payload)?;

    let mut hasher = Sha256::new();

    // Hashing the payload
    // Domain separation tag
    hasher.update(b"midnight-l2::batch_data");
    hasher.update(&payload);
    let payload_hash = hasher.finalize();
    let payload_hash = format!("{:x}", payload_hash);

    // Call the C++ AttestationClient program with the serialized and encoded payload.
    // Sudo is necessary as AttestationClient will read the vTPM values, most especially the OS values, for the attestation.
    let result = Command::new("sudo")
        .arg(client)
        .arg("-i")
        .arg(payload_hash)
        .arg("-n")
        .arg(nonce)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);

    // Check if the command was successful.
    if !result.status.success() {
        anyhow::bail!(
            "AttestationClient exited with status {}. stdout:\n{} stderr:\n{}",
            result.status,
            stdout,
            stderr
        );
    }
    Ok(stdout.to_string())
}

/// Function to verify the provided payload using the MAA service.
/// # Arguments
/// * `payload` - The JWT token to be verified.
/// * `policy` - The policy JSON to be used for verification.
pub fn verify(payload: &String, policy: &String, nonce: &str) -> Result<()> {
    let client = attestation_client_path()?;
    let result = Command::new(client)
        .arg("-p")
        .arg(policy)
        .arg("-v")
        .arg(payload)
        .arg("-n")
        .arg(nonce)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);

    // Check if the command was successful.
    if !result.status.success() {
        anyhow::bail!(
            "AttestationClient exited with status {}. stdout:\n{} stderr:\n{}",
            result.status,
            stdout,
            stderr
        );
    }
    println!("AttestationClient stdout: {}", stdout);
    println!("AttestationClient stderr: {}", stderr);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;
    const TEST_POLICY_JSON: &str = include_str!("../../policy_sample.json");

    #[test]
    fn test_attest() {
        // This test only exercises the attestation generation and verification logic.
        //
        // It does NOT provide protection against request forwarding or active
        // man-in-the-middle attacks. In this test, possession of a valid attestation
        // JWT is sufficient to pass verification.

        // Create a test payload.
        let payload = BatchPublicDataV1 {
            version: 1,
            layer2_chain_id: 1,
            batch_index: 1,
            da_start_height: 100,
            da_end_height: 200,
            da_commitment: [1u8; 32],
            prev_state_root: [0u8; 64],
            post_state_root: [0u8; 64],
            prev_batch_hash: [0u8; 32],
            batch_hash: [0u8; 32],
            last_processed_queue_index: U256::from(12844u64),
            message_queue_hash: [0u8; 32],
            withdraw_root: [8u8; 32],
        };
        // Generate the attestation.
        let result = attest(&payload, "midnight-l2");
        assert!(result.is_ok(), "Attestation failed");

        // If it was successful, we should have a JWT token.
        let attestation = result.unwrap();
        println!("Attestation result: {}", attestation);

        // Verify the attestation.
        println!("Verifying attestation using policy {}", TEST_POLICY_JSON);
        let result: std::result::Result<(), anyhow::Error> =
            verify(&attestation, &TEST_POLICY_JSON.to_string(), "midnight-l2");
        assert!(result.is_ok(), "Verification failed");

        // Decode the JWT payload to verify it contains the expected data.
        let jwt = jwt_payload_json(&attestation).unwrap();
        println!("JWT payload: {:?}", jwt);
        let client_payload_hash = jwt
            .pointer("/x-ms-runtime/client-payload/midnight_payload")
            .and_then(|s| s.as_str())
            .expect("Payload is empty");
        assert!(!client_payload_hash.is_empty(), "Expected payload is empty");
        // Decode the client payload.
        // Needed, as MAA encode the client payload to base64 before including it in the JWT.
        println!("Client payload: {:?}", client_payload_hash);
        let client_payload_hash = crate::common::BASE64_ENGINE
            .decode(client_payload_hash)
            .unwrap();

        // Get back the original payload.
        let client_payload_hash = String::from_utf8(client_payload_hash).unwrap();
        println!("Client payload (decoded): {:?}", client_payload_hash);

        // Hashing the original payload for comparison.
        let mut hasher = Sha256::new();
        let original_payload_bytes = to_vec(&payload).unwrap();
        // Domain separation tag
        hasher.update(b"midnight-l2::batch_data");
        hasher.update(&original_payload_bytes);
        let original_payload_hash = hasher.finalize();
        let original_payload_hash = format!("{:x}", original_payload_hash);

        // Check if the client payload is the same as the original payload.
        assert_eq!(
            original_payload_hash, client_payload_hash,
            "Mismatched payload hash"
        );
    }
}
