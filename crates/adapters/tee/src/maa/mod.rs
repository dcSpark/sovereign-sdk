use std::{env, path::PathBuf, process::{Command, Stdio}};
use rkyv::rancor::Error as RancorError;
use base64::{Engine as _, engine::{self, general_purpose, general_purpose::URL_SAFE_NO_PAD}, alphabet};
use anyhow::{Context, Result};
use serde_json::Value;
use crate::common::AttestationData;

// base64 engine to handle the encoding and decoding of the payload.
const BASE64_ENGINE: engine::GeneralPurpose = engine::GeneralPurpose::new(&alphabet::URL_SAFE, general_purpose::NO_PAD);

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
    let exe = env::current_exe()
        .context("cannot get current executable path")?;

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
pub fn attest(payload: AttestationData) -> Result<String> { 
    // As MAA requires a C++ library, we call an external C++ program to handle the attestation process.
    // Easier and less time consuming than writing bindings...

    // Get the path to the AttestationClient executable.
    let client = attestation_client_path()?;

    // Serialize the payload to a byte array using rkyv, then encode it to base64.
    let payload = rkyv::to_bytes::<RancorError>(&payload)?;
    let payload = BASE64_ENGINE.encode(payload);

    println!("Sending payload to AttestationClient: {}", payload);

    // Call the C++ AttestationClient program with the serialized and encoded payload.
    // Sudo is necessary as AttestationClient will read the vTPM values, most especially the OS values, for the attestation.
    let result = Command::new("sudo")
        .arg(client)
        .arg("-i")
        .arg(payload)
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
pub fn verify(payload: &String, policy: String) -> Result<()> { 
    let client = attestation_client_path()?;
    let result = Command::new(client)
        .arg("-p")
        .arg(policy)
        .arg("-v")
        .arg(payload)
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
    use super::*;
    const TEST_POLICY_JSON: &str = include_str!("../../policy_sample.json");

    #[test]
    fn test_attest() {
        // Create a test payload.
        let payload = AttestationData {
            prev_state_root: vec![0; 256],
            post_state_root: vec![0; 256],
            batch_hash: "0x1234567890abcdef".to_string(),
            message_queue_hash: "0x1234567890abcdef".to_string(),
            batch_index: 1,
            layer2_chain_id: "0x1234567890abcdef".to_string(),
        };
        // Generate the attestation.
        let result = attest(payload.clone());
        assert!(result.is_ok(), "Attestation failed");

        // If it was successful, we should have a JWT token.
        let attestation = result.unwrap();
        println!("Attestation result: {}", attestation);

        // Verify the attestation.
        println!("Verifying attestation using policy {}", TEST_POLICY_JSON);
        let result = verify(&attestation, TEST_POLICY_JSON.to_string());
        assert!(result.is_ok(), "Verification failed");

        // Decode the JWT payload to verify it contains the expected data.
        let jwt = jwt_payload_json(&attestation).unwrap();
        println!("JWT payload: {:?}", jwt);
        let client_payload = jwt.pointer("/x-ms-runtime/client-payload/midnight_payload").and_then(|s| s.as_str()).expect("Payload is empty");
        assert!(!client_payload.is_empty(), "Expected payload is empty");

        // Decode the client payload.
        // Needed, as the payload is encoded twice, once by us, once by MAA...
        let client_payload = BASE64_ENGINE.decode(client_payload).unwrap();
        let client_payload = BASE64_ENGINE.decode(client_payload).unwrap();
        // Get back the original payload.
        let client_payload: AttestationData = rkyv::from_bytes::<AttestationData, RancorError>(&client_payload).unwrap();
        println!("Client payload: {:?}", client_payload);
        // Check if the client payload is the same as the original payload.
        assert_eq!(payload, client_payload, "Extracted payload does not match the original payload");
    }
}