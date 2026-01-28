//! Zero-knowledge proof generation module for value-setter-zk transactions
//!
//! This module provides functionality to generate Ligero proofs that demonstrate
//! a value is within a valid range without revealing the computation details.

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

/// Program argument encoding expected by the Ligero prover/verifier JSON interface.
pub use ligero_runner::LigeroArg as LigeroProgramArguments;

/// Minimal wrapper used by MCP to generate Ligero proofs.
///
/// Proof generation is delegated to the external `ligero-http-server`.
#[derive(Debug, Clone)]
pub struct Ligero {
    proof_service_url: String,
    /// Circuit name or program specifier understood by the proof service.
    circuit: String,
    http: HttpClient,
}

impl Ligero {
    pub fn new(proof_service_url: String, circuit: String) -> Self {
        let proof_service_url = normalize_base_url(&proof_service_url);
        Self {
            proof_service_url,
            circuit,
            http: HttpClient::new(),
        }
    }

    pub fn proof_service_url(&self) -> &str {
        &self.proof_service_url
    }

    /// Generate a proof with automatic handling of string arguments
    /// This is a convenience wrapper that converts string args to the appropriate format
    #[allow(dead_code)]
    pub async fn generate_proof_with_public_output<T: Serialize>(
        &self,
        private_indices: Vec<u32>,
        args: Vec<String>,
        public_output: &T,
    ) -> Result<Vec<u8>> {
        // Convert string args to LigeroProgramArguments
        let ligero_args: Vec<LigeroProgramArguments> = args
            .into_iter()
            .map(|s| LigeroProgramArguments::String { str: s })
            .collect();

        // For midnight-privacy, we need to serialize the public output and include it
        // The public output is handled by the guest program, so we just generate the proof normally
        let _ = public_output; // Public output is validated by guest, not passed explicitly

        self.generate_proof(private_indices, ligero_args).await
    }

    pub async fn generate_proof(
        &self,
        private_indices: Vec<u32>,
        args: Vec<LigeroProgramArguments>,
    ) -> Result<Vec<u8>> {
        use std::time::Instant;

        let circuit = self.circuit.trim();
        anyhow::ensure!(
            !circuit.is_empty(),
            "ligero circuit is required (config.ligero_program_path or LIGERO_PROGRAM_PATH)"
        );

        let base_url = self.proof_service_url.trim();
        anyhow::ensure!(
            !base_url.is_empty(),
            "ligero proof service URL is required (LIGERO_PROOF_SERVICE_URL)"
        );

        let endpoint = if base_url.ends_with("/prove") {
            base_url.to_string()
        } else {
            format!("{}/prove", base_url)
        };

        let num_args = args.len();
        let request = ProveRequest {
            circuit: circuit.to_string(),
            args,
            proof: None,
            private_indices,
        };

        let request_start = Instant::now();
        let response = self
            .http
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("POST {endpoint}"))?
            .error_for_status()
            .with_context(|| format!("POST {endpoint} returned error status"))?;
        let http_ms = request_start.elapsed().as_millis();

        let parse_start = Instant::now();
        let payload: ProveResponse = response
            .json()
            .await
            .context("Failed to deserialize Ligero proof service response")?;
        let parse_ms = parse_start.elapsed().as_millis();

        if !payload.success || payload.exit_code != 0 {
            let error = payload.error.unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!(
                "Ligero proof service failed (exitCode={}): {}",
                payload.exit_code,
                error
            );
        }

        let proof_b64 = payload
            .proof
            .context("Ligero proof service response missing proof payload")?;
        let proof_b64 = proof_b64.trim();
        let decode_start = Instant::now();
        let proof_bytes = general_purpose::STANDARD
            .decode(proof_b64)
            .context("Failed to decode base64 proof payload")?;
        let decode_ms = decode_start.elapsed().as_millis();

        tracing::info!(
            endpoint,
            num_args,
            http_ms,
            parse_ms,
            decode_ms,
            proof_bytes_len = proof_bytes.len(),
            "[LIGERO_TIMING] Proof service call completed"
        );

        Ok(proof_bytes)
    }
}

fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProveRequest {
    circuit: String,
    args: Vec<LigeroProgramArguments>,
    proof: Option<String>,
    private_indices: Vec<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProveResponse {
    success: bool,
    exit_code: i32,
    proof: Option<String>,
    error: Option<String>,
}
