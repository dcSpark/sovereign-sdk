//! Nightstream proof generation for Midnight Privacy transfers.
//!
//! Delegates proof generation to an external Nightstream prover service that
//! accepts SpendPublic and returns serialized proof bytes.

use anyhow::{Context, Result};
use midnight_privacy::SpendPublic;
use reqwest::Client as HttpClient;
use serde::Serialize;

/// Minimal wrapper used by MCP to generate Nightstream proofs.
///
/// Proof generation is delegated to the external Nightstream prover service.
#[derive(Debug, Clone)]
pub struct Nightstream {
    proof_service_url: String,
    /// Circuit/program identifier (e.g. "note_spend_guest").
    circuit: String,
    http: HttpClient,
}

impl Nightstream {
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

    /// Generate a proof by sending SpendPublic to the Nightstream prover service.
    pub async fn generate_proof(&self, public: &SpendPublic) -> Result<Vec<u8>> {
        let base_url = self.proof_service_url.trim();
        anyhow::ensure!(
            !base_url.is_empty(),
            "Nightstream proof service URL is required (NIGHTSTREAM_PROOF_SERVICE_URL)"
        );

        let endpoint = if base_url.ends_with("/prove") {
            base_url.to_string()
        } else {
            format!("{}/prove", base_url)
        };

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ProveRequest<'a> {
            spend_public: &'a SpendPublic,
            binary: bool,
        }

        let request = ProveRequest {
            spend_public: public,
            binary: true,
        };

        let response = self
            .http
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("POST {endpoint}"))?
            .error_for_status()
            .with_context(|| format!("POST {endpoint} returned error status"))?;

        let proof_bytes = response
            .bytes()
            .await
            .context("Failed to read proof bytes from response")?
            .to_vec();

        anyhow::ensure!(
            !proof_bytes.is_empty(),
            "Nightstream proof service returned empty proof payload"
        );

        Ok(proof_bytes)
    }
}

fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}
