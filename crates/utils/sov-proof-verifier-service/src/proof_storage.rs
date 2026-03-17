use anyhow::{Context, Result};
use chrono::{Datelike, Utc};
use s3::{creds::Credentials, Bucket, Region};
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
    bucket: Box<Bucket>,
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

        let region = match std::env::var("SOV_PROOF_VERIFIER_PROOF_ENDPOINT") {
            Ok(endpoint) if !endpoint.trim().is_empty() => Region::Custom {
                region: region_name.clone(),
                endpoint: endpoint.trim().to_string(),
            },
            _ => region_name
                .parse::<Region>()
                .with_context(|| format!("Invalid S3 region '{region_name}'"))?,
        };

        let credentials = Credentials::new(None, None, None, None, None)
            .context("Failed to load AWS credentials for proof storage")?;

        let bucket = Bucket::new(&bucket_name, region, credentials)
            .context("Failed to create S3 bucket client for proof storage")?;

        Ok(Some(Self {
            bucket_name,
            key_prefix,
            download_ttl_secs,
            bucket,
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
        let path = format!("/{}", key);

        let response = self
            .bucket
            .put_object_with_content_type(&path, &proof_bytes, "application/octet-stream")
            .await
            .with_context(|| format!("Failed to store proof package in s3://{}/{}", self.bucket_name, key))?;

        anyhow::ensure!(
            (200..300).contains(&response.status_code()),
            "Unexpected S3 status while storing proof package in s3://{}/{}: {}",
            self.bucket_name,
            key,
            response.status_code()
        );

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

        let path = format!("/{}", proof_ref.key.trim_start_matches('/'));
        let response = self
            .bucket
            .get_object(&path)
            .await
            .with_context(|| format!("Failed to fetch proof package from s3://{}/{}", self.bucket_name, proof_ref.key))?;

        anyhow::ensure!(
            (200..300).contains(&response.status_code()),
            "Unexpected S3 status while fetching proof package from s3://{}/{}: {}",
            self.bucket_name,
            proof_ref.key,
            response.status_code()
        );

        let bytes = response.to_vec();
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

        let path = format!("/{}", proof_ref.key.trim_start_matches('/'));
        self.bucket
            .presign_get(&path, self.download_ttl_secs, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to presign proof download URL for s3://{}/{}",
                    self.bucket_name, proof_ref.key
                )
            })
    }
}
