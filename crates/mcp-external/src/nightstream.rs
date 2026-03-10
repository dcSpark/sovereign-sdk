//! Nightstream proof generation for Midnight Privacy transfers.
//!
//! Delegates proof generation to an external Nightstream prover service that
//! accepts a full NoteSpendWitness and returns serialized proof bytes.

use anyhow::{Context, Result};
use base64::Engine;
use midnight_privacy::SpendPublic;
use reqwest::Client as HttpClient;
use serde::Serialize;
use sov_nightstream_adapter::NoteSpendWitness;

/// Minimal wrapper used by MCP to generate Nightstream proofs.
///
/// Proof generation is delegated to the external Nightstream prover service.
#[derive(Debug, Clone)]
pub struct Nightstream {
    proof_service_url: String,
    /// Circuit/program identifier (e.g. "note_spend_guest").
    #[allow(dead_code)]
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

    /// Generate a proof by sending a full NoteSpendWitness + SpendPublic to
    /// the Nightstream prover service.
    pub async fn generate_proof(
        &self,
        witness: &NoteSpendWitness,
        public: &SpendPublic,
    ) -> Result<Vec<u8>> {
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

        let public_output_bytes =
            bincode::serialize(public).context("Failed to bincode-serialize SpendPublic")?;
        let public_output =
            base64::engine::general_purpose::STANDARD.encode(&public_output_bytes);

        #[derive(Serialize)]
        struct ProveRequest<'a> {
            witness: &'a NoteSpendWitness,
            public_output: String,
            binary: bool,
        }

        let request = ProveRequest {
            witness,
            public_output,
            binary: true,
        };

        let response = self
            .http
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("POST {endpoint}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read response body: {e})"));
            anyhow::bail!("POST {endpoint} returned {status}: {body}");
        }

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

/// Inject a pre-computed pool viewer signature into DEFLATE-compressed proof
/// package bytes. The `pool_sig_hex` is the hex-encoded Ed25519 signature (64
/// bytes) and `fvk_commitment` is the 32-byte FVK commitment it covers.
pub fn inject_pool_viewer_sig(
    proof_bytes: Vec<u8>,
    fvk_commitment: [u8; 32],
    pool_sig_hex: &str,
) -> anyhow::Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use sov_nightstream_adapter::{NightstreamProofPackage, PoolViewerSig};
    use std::io::{Read, Write};

    let sig_bytes = hex::decode(pool_sig_hex.trim())
        .context("pool_sig_hex is not valid hex")?;
    anyhow::ensure!(
        sig_bytes.len() == 64,
        "pool_sig_hex must decode to 64 bytes (got {})",
        sig_bytes.len()
    );

    let decompressed = {
        let mut decoder = DeflateDecoder::new(proof_bytes.as_slice());
        let mut buf = Vec::new();
        decoder
            .read_to_end(&mut buf)
            .context("Failed to decompress proof bytes")?;
        buf
    };

    let mut package: NightstreamProofPackage =
        bincode::deserialize(&decompressed)
            .context("Failed to deserialize NightstreamProofPackage")?;

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
