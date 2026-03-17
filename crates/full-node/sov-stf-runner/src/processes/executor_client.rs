//! HTTP client for the Bridge executor service.
//!
//! The executor service (developed in another repo) exposes endpoints to submit
//! Bridge contract calls (commitBatch, finalizeBatch). This client matches the
//! API contract; no code is imported from the reference implementation.

use alloy_primitives::U256;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tee::common::BatchPublicDataV1;

fn base_url_normalized(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<T> {
    let url = format!("{}{}", base_url_normalized(base_url), path);
    let res = client
        .post(&url)
        .json(body)
        .send()
        .await
        .context("executor HTTP request failed")?;
    let status = res.status();
    let body_bytes = res
        .bytes()
        .await
        .context("executor response body read failed")?;
    if !status.is_success() {
        let msg = String::from_utf8_lossy(&body_bytes);
        let err_msg: String = serde_json::from_str::<serde_json::Value>(&msg)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| msg.into_owned());
        anyhow::bail!("executor {}: {} (status {})", path, err_msg.trim(), status);
    }
    serde_json::from_slice(&body_bytes).context("executor response JSON parse failed")
}

/// Key batch fields from the executor's Bridge contract state (GET /state).
#[derive(Debug, Clone)]
pub struct ExecutorBridgeState {
    pub last_finalized_batch_index: u64,
    pub last_finalized_batch_hash: [u8; 32],
    pub last_committed_batch_index: u64,
    pub last_committed_batch_hash: [u8; 32],
}

/// HTTP client for the Bridge executor service.
#[derive(Clone)]
pub struct ExecutorClient {
    client: reqwest::Client,
    base_url: String,
}

impl ExecutorClient {
    /// Creates a new executor client.
    pub fn new(client: reqwest::Client, base_url: String) -> Self {
        Self { client, base_url }
    }

    /// Submits commitBatch to the executor (POST /commit-batch).
    /// Hashes must be 32-byte values; they are sent as 64-char hex strings.
    pub async fn commit_batch(
        &self,
        parent_batch_hash: &[u8; 32],
        batch_hash: &[u8; 32],
    ) -> Result<()> {
        let body = json!({
            "parentHash": hex::encode(parent_batch_hash),
            "batchHash": hex::encode(batch_hash),
        });
        let _: serde_json::Value = post_json(&self.client, &self.base_url, "/commit-batch", &body).await?;
        Ok(())
    }

    /// Asks the executor to build signatures for the given batch public data (POST /build-signatures).
    /// Returns the `signatures` JSON string to pass to finalize_batch.
    pub async fn build_signatures(
        &self,
        batch_public_data_json: &str,
        signature_max_nonce: u64,
    ) -> Result<String> {
        let batch_public_data: serde_json::Value =
            serde_json::from_str(batch_public_data_json).context("batch_public_data JSON")?;
        let body = json!({
            "batchPublicData": batch_public_data,
            "signatureMaxNonce": signature_max_nonce,
        });
        #[derive(serde::Deserialize)]
        struct Out {
            signatures: String,
        }
        let out: Out = post_json(&self.client, &self.base_url, "/build-signatures", &body).await?;
        Ok(out.signatures)
    }

    /// Fetches Bridge contract state from the executor (GET /state).
    /// Returns key batch-related fields for diagnostic cross-checks.
    pub async fn get_state(&self) -> Result<ExecutorBridgeState> {
        let url = format!("{}/state", base_url_normalized(&self.base_url));
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .context("executor GET /state request failed")?;
        let status = res.status();
        let body_bytes = res
            .bytes()
            .await
            .context("executor /state response body read failed")?;
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&body_bytes);
            anyhow::bail!("executor /state: {} (status {})", msg.trim(), status);
        }
        let raw: serde_json::Value =
            serde_json::from_slice(&body_bytes).context("executor /state JSON parse failed")?;
        let last_finalized_batch_index = raw
            .get("lastFinalizedBatchIndex")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let last_finalized_batch_hash = raw
            .get("lastFinalizedBatchHash")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                let s = s.strip_prefix("0x").unwrap_or(s);
                hex::decode(s).ok()
            })
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .unwrap_or([0u8; 32]);
        let last_committed_batch_index = raw
            .get("lastCommittedBatchIndex")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let last_committed_batch_hash = raw
            .get("lastCommittedBatchHash")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                let s = s.strip_prefix("0x").unwrap_or(s);
                hex::decode(s).ok()
            })
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .unwrap_or([0u8; 32]);
        Ok(ExecutorBridgeState {
            last_finalized_batch_index,
            last_finalized_batch_hash,
            last_committed_batch_index,
            last_committed_batch_hash,
        })
    }

    /// Submits finalizeBatch to the executor (POST /finalize-batch).
    pub async fn finalize_batch(
        &self,
        batch_public_data_json: &str,
        signatures_json: &str,
        signer_bitmap: u8,
        finalize_timestamp: u64,
    ) -> Result<()> {
        let batch_public_data: serde_json::Value =
            serde_json::from_str(batch_public_data_json).context("batch_public_data JSON")?;
        let signatures: serde_json::Value =
            serde_json::from_str(signatures_json).context("signatures JSON")?;
        let body = json!({
            "batchPublicData": batch_public_data,
            "signatures": signatures,
            "signerBitmap": signer_bitmap,
            "finalizeTimestamp": finalize_timestamp,
        });
        let _: serde_json::Value =
            post_json(&self.client, &self.base_url, "/finalize-batch", &body).await?;
        Ok(())
    }
}

/// JSON shape for BatchPublicDataV1Full as expected by the executor (camelCase, 32-byte fields as hex).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchPublicDataV1FullJson {
    version: u32,
    layer2_chain_id: u64,
    rollup_id: String,
    batch_index: u64,
    da_start_height: u64,
    da_end_height: u64,
    da_commitment: String,
    last_processed_queue_index: u64,
    message_queue_hash: String,
    prev_state_root: String,
    prev_batch_hash: String,
    post_state_root: String,
    batch_hash: String,
    withdraw_root: String,
}

fn bytes32_to_hex(b: &[u8; 32]) -> String {
    hex::encode(b)
}

/// Converts BatchPublicDataV1 and rollup_id to the executor's BatchPublicDataV1Full JSON string.
/// State roots in BatchPublicDataV1 are 64 bytes; the contract expects 32, so we use the first 32 bytes.
pub fn batch_public_data_to_executor_json(
    batch: &BatchPublicDataV1,
    rollup_id: &[u8; 32],
) -> Result<String> {
    let last_processed = batch
        .last_processed_queue_index
        .min(U256::from(u64::MAX))
        .to::<u64>();
    let prev_state_root_32: [u8; 32] = batch.prev_state_root[..32]
        .try_into()
        .map_err(|_| anyhow::anyhow!("prev_state_root too short"))?;
    let post_state_root_32: [u8; 32] = batch.post_state_root[..32]
        .try_into()
        .map_err(|_| anyhow::anyhow!("post_state_root too short"))?;
    let j = BatchPublicDataV1FullJson {
        version: batch.version,
        layer2_chain_id: batch.layer2_chain_id,
        rollup_id: bytes32_to_hex(rollup_id),
        batch_index: batch.batch_index,
        da_start_height: batch.da_start_height,
        da_end_height: batch.da_end_height,
        da_commitment: bytes32_to_hex(&batch.da_commitment),
        last_processed_queue_index: last_processed,
        message_queue_hash: bytes32_to_hex(&batch.message_queue_hash),
        prev_state_root: bytes32_to_hex(&prev_state_root_32),
        prev_batch_hash: bytes32_to_hex(&batch.prev_batch_hash),
        post_state_root: bytes32_to_hex(&post_state_root_32),
        batch_hash: bytes32_to_hex(&batch.batch_hash),
        withdraw_root: bytes32_to_hex(&batch.withdraw_root),
    };
    serde_json::to_string(&j).context("batch public data JSON serialize")
}
