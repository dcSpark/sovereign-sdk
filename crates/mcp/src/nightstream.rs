//! Nightstream proof generation for Midnight Privacy transfers.
//!
//! Delegates proof generation to an external Nightstream prover service that
//! accepts a full NoteSpendWitness and returns serialized proof bytes.

use anyhow::{Context, Result};
use base64::Engine;
use midnight_privacy::SpendPublic;
use reqwest::Client as HttpClient;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofRef {
    pub bucket: String,
    pub key: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub encoding: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GeneratedProof {
    /// Inline proof bytes when the prover returns them directly. When `proof_ref`
    /// is present, MCP can keep the transaction lightweight and avoid fetching
    /// the proof package from object storage.
    pub proof_bytes: Vec<u8>,
    pub proof_ref: Option<ProofRef>,
}

#[derive(Debug, Clone)]
pub struct PoolViewerSignature {
    pub fvk_commitment: [u8; 32],
    pub pool_sig_hex: String,
}

#[derive(Debug, Deserialize)]
struct ProveResponse {
    success: bool,
    #[serde(rename = "exitCode")]
    #[allow(dead_code)]
    exit_code: i32,
    #[serde(default)]
    proof: Option<String>,
    #[serde(default)]
    proof_ref: Option<ProofRef>,
    #[serde(default)]
    error: Option<String>,
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

    #[allow(dead_code)]
    pub fn proof_service_url(&self) -> &str {
        &self.proof_service_url
    }

    /// Generate a proof by sending a full NoteSpendWitness + SpendPublic to
    /// the Nightstream prover service.
    pub async fn generate_proof(
        &self,
        witness: &NoteSpendWitness,
        public: &SpendPublic,
        pool_viewer_signature: Option<&PoolViewerSignature>,
    ) -> Result<GeneratedProof> {
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
        let public_output = base64::engine::general_purpose::STANDARD.encode(&public_output_bytes);

        #[derive(Serialize)]
        struct ProveRequest<'a> {
            witness: &'a NoteSpendWitness,
            public_output: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            pool_viewer_sig_hex: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pool_viewer_fvk_commitment: Option<String>,
        }

        let request = ProveRequest {
            witness,
            public_output,
            pool_viewer_sig_hex: pool_viewer_signature.map(|sig| sig.pool_sig_hex.as_str()),
            pool_viewer_fvk_commitment: pool_viewer_signature
                .map(|sig| hex::encode(sig.fvk_commitment)),
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

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        if !content_type.contains("application/json") {
            let proof_bytes = response
                .bytes()
                .await
                .context("Failed to read proof bytes from response")?
                .to_vec();
            anyhow::ensure!(
                !proof_bytes.is_empty(),
                "Nightstream proof service returned empty proof payload"
            );
            return Ok(GeneratedProof {
                proof_bytes,
                proof_ref: None,
            });
        }

        let response_body: ProveResponse = response
            .json()
            .await
            .context("Failed to deserialize /prove response")?;

        anyhow::ensure!(
            response_body.success,
            "{}",
            response_body
                .error
                .unwrap_or_else(|| "Nightstream proof service reported failure".to_string())
        );

        if let Some(proof_b64) = response_body.proof {
            let proof_bytes = base64::engine::general_purpose::STANDARD
                .decode(proof_b64)
                .context("Failed to decode base64 proof returned by proof service")?;
            return Ok(GeneratedProof {
                proof_bytes,
                proof_ref: response_body.proof_ref,
            });
        }

        let proof_ref = response_body
            .proof_ref
            .context("Proof service returned neither proof bytes nor proof_ref")?;
        Ok(GeneratedProof {
            // Build the signed tx without embedding the large proof. The verifier
            // service will re-attach it from object storage using `proof_ref`.
            proof_bytes: Vec::new(),
            proof_ref: Some(proof_ref),
        })
    }
}

fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Inject a pre-computed pool viewer signature into DEFLATE-compressed proof
/// package bytes. The `pool_sig_hex` is the hex-encoded Ed25519 signature (64
/// bytes) and `fvk_commitment` is the 32-byte FVK commitment it covers.
#[allow(dead_code)]
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

    let sig_bytes = hex::decode(pool_sig_hex.trim()).context("pool_sig_hex is not valid hex")?;
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

    let mut package: NightstreamProofPackage = bincode::deserialize(&decompressed)
        .context("Failed to deserialize NightstreamProofPackage")?;

    package.pool_viewer_sig = Some(PoolViewerSig {
        fvk_commitment,
        signature: sig_bytes,
    });

    let raw =
        bincode::serialize(&package).context("Failed to re-serialize NightstreamProofPackage")?;

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&raw)
        .context("Failed to write to deflate encoder")?;
    encoder
        .finish()
        .context("Failed to finish deflate compression")
}
