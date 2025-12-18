use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;

use crate::{IncomingWorkerTxSaveMode, MidnightDaConfig};

/// Persist full incoming worker transactions (the base64-encoded borsh tx bytes).
///
/// This is intended for debugging/auditing and is separate from the
/// `worker_verified_transactions` DB table.
#[derive(Clone)]
pub struct IncomingWorkerTxSaver {
    inner: Arc<IncomingWorkerTxSaverInner>,
}

enum IncomingWorkerTxSaverInner {
    Disabled,
    Disk { dir: PathBuf },
    Gcs {
        bucket: String,
        storage: google_cloud_storage::client::Storage,
    },
}

impl IncomingWorkerTxSaver {
    /// Creates a saver that does nothing.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(IncomingWorkerTxSaverInner::Disabled),
        }
    }

    /// Creates a saver from a rollup's `[da]` config.
    ///
    /// If `config_dir` is provided, relative `worker_tx_path` values are resolved against it.
    pub async fn from_config(
        da_config: &MidnightDaConfig,
        config_dir: Option<&Path>,
    ) -> anyhow::Result<Self> {
        match da_config.save_incoming_worker_txs {
            IncomingWorkerTxSaveMode::None => Ok(Self::disabled()),
            IncomingWorkerTxSaveMode::Disk => {
                let raw_path = da_config.worker_tx_path.as_deref().context(
                    "Midnight DA config requires `worker_tx_path` when `save_incoming_worker_txs = \"disk\"`",
                )?;
                let dir = resolve_path(config_dir, raw_path);
                tokio::fs::create_dir_all(&dir)
                    .await
                    .with_context(|| format!("Failed to create worker tx directory {}", dir.display()))?;
                Ok(Self {
                    inner: Arc::new(IncomingWorkerTxSaverInner::Disk { dir }),
                })
            }
            IncomingWorkerTxSaveMode::Gcs => {
                let raw_bucket = da_config.worker_tx_bucket.as_deref().context(
                    "Midnight DA config requires `worker_tx_bucket` when `save_incoming_worker_txs = \"gcs\"`",
                )?;
                let bucket = normalize_gcs_bucket(raw_bucket);
                let storage = google_cloud_storage::client::Storage::builder()
                    .build()
                    .await
                    .context("Failed to initialize GCS client")?;
                Ok(Self {
                    inner: Arc::new(IncomingWorkerTxSaverInner::Gcs { bucket, storage }),
                })
            }
        }
    }

    /// Persists a transaction blob according to the configured backend.
    pub async fn save(&self, tx_hash: &str, full_transaction_blob_base64: &str) -> anyhow::Result<()> {
        let object_name = format!("{tx_hash}.json");
        let payload = serde_json::to_vec_pretty(&serde_json::json!({
            "tx_hash": tx_hash,
            "full_transaction_blob": full_transaction_blob_base64,
        }))
        .with_context(|| format!("Failed to serialize worker tx JSON for {tx_hash}"))?;

        match self.inner.as_ref() {
            IncomingWorkerTxSaverInner::Disabled => Ok(()),
            IncomingWorkerTxSaverInner::Disk { dir } => {
                let path = dir.join(&object_name);
                tokio::fs::write(&path, payload)
                    .await
                    .with_context(|| format!("Failed to write worker tx file {}", path.display()))
            }
            IncomingWorkerTxSaverInner::Gcs { bucket, storage } => {
                storage
                    .write_object(bucket, &object_name, bytes::Bytes::from(payload))
                    .set_content_type("application/json")
                    .send_buffered()
                    .await
                    .with_context(|| format!("Failed to upload worker tx object gs://{bucket}/{object_name}"))?;
                Ok(())
            }
        }
    }
}

fn resolve_path(config_dir: Option<&Path>, raw_path: &str) -> PathBuf {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        return path.to_path_buf();
    }

    let base = config_dir.unwrap_or_else(|| Path::new("."));
    base.join(path)
}

fn normalize_gcs_bucket(bucket: &str) -> String {
    let bucket = bucket
        .strip_prefix("gs://")
        .unwrap_or(bucket)
        .trim_matches('/');

    if bucket.starts_with("projects/") {
        bucket.to_string()
    } else {
        format!("projects/_/buckets/{bucket}")
    }
}
