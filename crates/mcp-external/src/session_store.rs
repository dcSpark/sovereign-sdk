use anyhow::{Context, Result};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use rand::RngCore;

use crate::fvk_service::{parse_hex_32, ViewerFvkBundle};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub wallet_private_key_hex: Option<String>,
    pub privacy_spend_key_hex: Option<String>,
    pub viewer_fvk_bundle: Option<ViewerFvkBundleStored>,
    pub wallet_explicitly_loaded: bool,
}

impl SessionSnapshot {
    pub fn empty() -> Self {
        Self {
            wallet_private_key_hex: None,
            privacy_spend_key_hex: None,
            viewer_fvk_bundle: None,
            wallet_explicitly_loaded: false,
        }
    }

    pub fn from_keys(
        wallet_private_key_hex: String,
        privacy_spend_key_hex: String,
        viewer_fvk_bundle: Option<ViewerFvkBundle>,
    ) -> Self {
        Self {
            wallet_private_key_hex: Some(wallet_private_key_hex),
            privacy_spend_key_hex: Some(privacy_spend_key_hex),
            viewer_fvk_bundle: viewer_fvk_bundle.map(ViewerFvkBundleStored::from),
            wallet_explicitly_loaded: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerFvkBundleStored {
    pub fvk: String,
    pub fvk_commitment: String,
    pub pool_sig_hex: String,
    pub signer_public_key: String,
    pub shielded_address: Option<String>,
    pub wallet_address: Option<String>,
}

impl From<ViewerFvkBundle> for ViewerFvkBundleStored {
    fn from(value: ViewerFvkBundle) -> Self {
        Self {
            fvk: hex::encode(value.fvk),
            fvk_commitment: hex::encode(value.fvk_commitment),
            pool_sig_hex: value.pool_sig_hex,
            signer_public_key: hex::encode(value.signer_public_key),
            shielded_address: value.shielded_address,
            wallet_address: value.wallet_address,
        }
    }
}

impl ViewerFvkBundleStored {
    pub fn try_into_bundle(self) -> Result<ViewerFvkBundle> {
        let fvk = parse_hex_32("viewer_fvk", &self.fvk)?;
        let fvk_commitment = parse_hex_32("viewer_fvk_commitment", &self.fvk_commitment)?;
        let signer_public_key = parse_hex_32("viewer_fvk_signer_public_key", &self.signer_public_key)?;
        Ok(ViewerFvkBundle {
            fvk,
            fvk_commitment,
            pool_sig_hex: self.pool_sig_hex,
            signer_public_key,
            shielded_address: self.shielded_address,
            wallet_address: self.wallet_address,
        })
    }
}

pub struct SessionStore {
    pool: PgPool,
    encryptor: Option<Encryptor>,
}

impl SessionStore {
    pub async fn connect(db_url: &str, encryption_key: Option<&str>) -> Result<Self> {
        let pool = PgPool::connect(db_url)
            .await
            .with_context(|| "Failed to connect to MCP session database")?;
        let encryptor = match encryption_key {
            Some(raw) => Some(Encryptor::new(parse_encryption_key(raw)?)?),
            None => None,
        };
        let store = Self { pool, encryptor };
        store.init().await?;
        Ok(store)
    }

    async fn init(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mcp_sessions (
                session_id TEXT PRIMARY KEY,
                payload BYTEA NOT NULL,
                encrypted BOOLEAN NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await
        .context("Failed to initialize MCP session table")?;
        Ok(())
    }

    pub async fn load_session(&self, session_id: &str) -> Result<Option<SessionSnapshot>> {
        let row = sqlx::query(
            "SELECT payload, encrypted FROM mcp_sessions WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to fetch MCP session payload")?;

        let Some(row) = row else { return Ok(None); };

        let payload: Vec<u8> = row.try_get("payload")?;
        let encrypted: bool = row.try_get("encrypted")?;
        let decoded = self.decode_payload(&payload, encrypted)?;
        let snapshot: SessionSnapshot = serde_json::from_slice(&decoded)
            .context("Failed to decode MCP session payload")?;
        Ok(Some(snapshot))
    }

    pub async fn save_session(&self, session_id: &str, snapshot: &SessionSnapshot) -> Result<()> {
        let encoded = serde_json::to_vec(snapshot).context("Failed to serialize session snapshot")?;
        let (payload, encrypted) = self.encode_payload(&encoded)?;
        sqlx::query(
            "INSERT INTO mcp_sessions (session_id, payload, encrypted, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (session_id)
             DO UPDATE SET payload = EXCLUDED.payload, encrypted = EXCLUDED.encrypted, updated_at = now()",
        )
        .bind(session_id)
        .bind(payload)
        .bind(encrypted)
        .execute(&self.pool)
        .await
        .context("Failed to persist MCP session snapshot")?;
        Ok(())
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM mcp_sessions WHERE session_id = $1")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .context("Failed to delete MCP session snapshot")?;
        Ok(())
    }

    fn encode_payload(&self, data: &[u8]) -> Result<(Vec<u8>, bool)> {
        match &self.encryptor {
            Some(encryptor) => Ok((encryptor.encrypt(data)?, true)),
            None => Ok((data.to_vec(), false)),
        }
    }

    fn decode_payload(&self, data: &[u8], encrypted: bool) -> Result<Vec<u8>> {
        if encrypted {
            let encryptor = self
                .encryptor
                .as_ref()
                .context("Session payload is encrypted but MCP_SESSION_DB_ENCRYPTION_KEY is not set")?;
            encryptor.decrypt(data)
        } else {
            Ok(data.to_vec())
        }
    }
}

struct Encryptor {
    cipher: Aes256Gcm,
}

impl Encryptor {
    fn new(key: [u8; 32]) -> Result<Self> {
        Ok(Self {
            cipher: Aes256Gcm::new_from_slice(&key)
                .context("Failed to initialize session encryption cipher")?,
        })
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(nonce.as_slice().into(), plaintext)
            .context("Failed to encrypt session snapshot")?;
        let mut payload = Vec::with_capacity(nonce.len() + ciphertext.len());
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ciphertext);
        Ok(payload)
    }

    fn decrypt(&self, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() < 12 {
            anyhow::bail!("Encrypted session payload is too short");
        }
        let (nonce, ciphertext) = payload.split_at(12);
        self.cipher
            .decrypt(nonce.into(), ciphertext)
            .context("Failed to decrypt session snapshot")
    }
}

fn parse_encryption_key(raw: &str) -> Result<[u8; 32]> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = hex::decode(trimmed).context("Invalid hex for MCP_SESSION_DB_ENCRYPTION_KEY")?;
        return bytes
            .as_slice()
            .try_into()
            .context("MCP_SESSION_DB_ENCRYPTION_KEY must be 32 bytes");
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .context("Invalid base64 for MCP_SESSION_DB_ENCRYPTION_KEY")?;
    decoded
        .as_slice()
        .try_into()
        .context("MCP_SESSION_DB_ENCRYPTION_KEY must be 32 bytes")
}
