use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_s3::{
    config::{Builder as S3ConfigBuilder, Region},
    presigning::PresigningConfig,
    primitives::ByteStream,
    Client,
};
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DEFAULT_DOWNLOAD_TTL_SECS: u32 = 900;
const DEFAULT_KEY_PREFIX: &str = "nightstream-proofs";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofReference {
    pub bucket: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredProof {
    pub proof_ref: ProofReference,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_download_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProofStorage {
    bucket_name: String,
    key_prefix: String,
    download_ttl_secs: u32,
    client: Client,
}

impl ProofStorage {
    pub async fn from_env() -> Result<Option<Self>> {
        let bucket_name = match std::env::var("SOV_PROOF_VERIFIER_PROOF_BUCKET") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => return Ok(None),
        };

        let key_prefix = std::env::var("SOV_PROOF_VERIFIER_PROOF_PREFIX")
            .ok()
            .map(|value| value.trim().trim_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_KEY_PREFIX.to_string());

        let download_ttl_secs = std::env::var("SOV_PROOF_VERIFIER_PROOF_DOWNLOAD_TTL_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_DOWNLOAD_TTL_SECS);

        let region_name = std::env::var("SOV_PROOF_VERIFIER_PROOF_REGION")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("AWS_REGION").ok())
            .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
            .unwrap_or_else(|| "us-east-1".to_string());

        let region = Region::new(region_name.clone());
        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(region.clone())
            .load()
            .await;

        let mut config_builder = S3ConfigBuilder::from(&shared_config)
            .region(region)
            .force_path_style(false);

        if let Ok(endpoint) = std::env::var("SOV_PROOF_VERIFIER_PROOF_ENDPOINT") {
            let endpoint = endpoint.trim();
            if !endpoint.is_empty() {
                config_builder = config_builder.endpoint_url(endpoint.to_string());
            }
        }

        let client = Client::from_conf(config_builder.build());

        Ok(Some(Self {
            bucket_name,
            key_prefix,
            download_ttl_secs,
            client,
        }))
    }

    pub fn bucket(&self) -> &str {
        &self.bucket_name
    }

    pub async fn store_proof(&self, proof_bytes: Vec<u8>) -> Result<StoredProof> {
        let sha256 = hex::encode(Sha256::digest(&proof_bytes));
        let now = Utc::now();
        let key = format!(
            "{}/{:04}/{:02}/{:02}/{}.bin",
            self.key_prefix,
            now.year(),
            now.month(),
            now.day(),
            sha256
        );

        self.client
            .put_object()
            .bucket(&self.bucket_name)
            .key(&key)
            .content_type("application/octet-stream")
            .body(ByteStream::from(proof_bytes.clone()))
            .send()
            .await
            .with_context(|| {
                format!(
                    "Failed to store proof package in s3://{}/{}",
                    self.bucket_name, key
                )
            })?;

        let proof_ref = ProofReference {
            bucket: self.bucket_name.clone(),
            key: key.clone(),
            sha256: Some(sha256),
            size_bytes: Some(proof_bytes.len() as u64),
            encoding: Some("deflate".to_string()),
        };

        let proof_download_url = self.presign_get(&proof_ref).await.ok();

        Ok(StoredProof {
            proof_ref,
            proof_download_url,
        })
    }

    pub async fn load_proof(&self, proof_ref: &ProofReference) -> Result<Vec<u8>> {
        anyhow::ensure!(
            proof_ref.bucket.trim().is_empty() || proof_ref.bucket == self.bucket_name,
            "Proof reference bucket '{}' does not match configured bucket '{}'",
            proof_ref.bucket,
            self.bucket_name
        );

        let response = self
            .client
            .get_object()
            .bucket(&self.bucket_name)
            .key(proof_ref.key.trim_start_matches('/'))
            .send()
            .await
            .with_context(|| {
                format!(
                    "Failed to fetch proof package from s3://{}/{}",
                    self.bucket_name, proof_ref.key
                )
            })?;

        let bytes = response
            .body
            .collect()
            .await
            .context("Failed to read proof package bytes from S3")?
            .into_bytes()
            .to_vec();

        if let Some(expected_sha256) = proof_ref.sha256.as_deref() {
            let actual_sha256 = hex::encode(Sha256::digest(&bytes));
            anyhow::ensure!(
                actual_sha256 == expected_sha256,
                "Proof package sha256 mismatch for s3://{}/{} (expected {}, got {})",
                self.bucket_name,
                proof_ref.key,
                expected_sha256,
                actual_sha256
            );
        }

        Ok(bytes)
    }

    pub async fn presign_get(&self, proof_ref: &ProofReference) -> Result<String> {
        anyhow::ensure!(
            proof_ref.bucket.trim().is_empty() || proof_ref.bucket == self.bucket_name,
            "Proof reference bucket '{}' does not match configured bucket '{}'",
            proof_ref.bucket,
            self.bucket_name
        );

        let config = PresigningConfig::expires_in(Duration::from_secs(
            self.download_ttl_secs.into(),
        ))
        .context("Invalid proof download presign TTL")?;

        let request = self
            .client
            .get_object()
            .bucket(&self.bucket_name)
            .key(proof_ref.key.trim_start_matches('/'))
            .presigned(config)
            .await
            .with_context(|| {
                format!(
                    "Failed to presign proof download URL for s3://{}/{}",
                    self.bucket_name, proof_ref.key
                )
            })?;

        Ok(request.uri().to_string())
    }
}
