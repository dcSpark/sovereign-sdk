use anyhow::{Context, Result};
use serde::de::Error as _;
use serde::Deserialize;
use std::path::PathBuf;
use validator::Validate;

fn deserialize_env_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: String = Deserialize::deserialize(deserializer)?;
    let v = raw.trim();
    if v.is_empty() {
        return Ok(true);
    }

    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" => Ok(false),
        _ => Err(D::Error::custom(format!(
            "invalid boolean value: {v} (expected one of: true/false/1/0/yes/no/on/off)"
        ))),
    }
}

#[derive(Debug, Clone, Deserialize, Validate)]
pub struct Config {
    /// Server bind address (env: ORACLE_SERVER_BIND_ADDRESS, default: "127.0.0.1:8080")
    #[serde(default = "default_server_bind_address")]
    #[validate(length(min = 1))]
    pub oracle_server_bind_address: String,

    /// Directory containing policy JSON files (env: ORACLE_POLICIES_DIR)
    #[serde(default = "default_policies_dir")]
    #[validate(length(min = 1))]
    pub oracle_policies_dir: String,

    /// Ed25519 signing key seed as hex (32 bytes, env: ORACLE_SIGNING_KEY_HEX).
    ///
    /// Either `oracle_signing_key_hex` or `oracle_signing_key_path` must be provided.
    #[serde(default)]
    pub oracle_signing_key_hex: Option<String>,

    /// Path to a file containing `ORACLE_SIGNING_KEY_HEX` (env: ORACLE_SIGNING_KEY_PATH).
    ///
    /// Either `oracle_signing_key_hex` or `oracle_signing_key_path` must be provided.
    #[serde(default)]
    pub oracle_signing_key_path: Option<PathBuf>,

    /// Development mode: skip policy validation and sign any request (env: ORACLE_DEV_ACCEPT_ALL, default: false).
    #[serde(default, deserialize_with = "deserialize_env_bool")]
    pub oracle_dev_accept_all: bool,

    /// Database connection string for storing attestations (env: ORACLE_DB_CONNECTION_STRING).
    /// Supports SQLite (sqlite://) or PostgreSQL (postgresql://).
    /// If not set, attestations will not be persisted.
    #[serde(default)]
    pub oracle_db_connection_string: Option<String>,

    /// Max entries per attestation LRU cache (env: ORACLE_CACHE_SIZE, default: 5000).
    #[serde(default = "default_cache_size")]
    pub oracle_cache_size: usize,
}

fn default_cache_size() -> usize {
    5000
}

fn default_server_bind_address() -> String {
    "0.0.0.0:8090".to_owned()
}

fn default_policies_dir() -> String {
    "./policies".to_owned()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let cfg: Self =
            envy::from_env().context("Failed to load configuration from environment variables")?;

        // Validate all fields using the validator derive macro
        if let Err(errors) = cfg.validate() {
            tracing::info!("\nConfiguration validation failed:");
            for (field, field_errors) in errors.field_errors() {
                for error in field_errors {
                    let message = error
                        .message
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| format!("Validation error: {}", error.code));
                    tracing::info!("  • {}: {}", field, message);
                }
            }
            anyhow::bail!("Configuration validation failed");
        }

        if cfg.oracle_signing_key_hex.is_none() && cfg.oracle_signing_key_path.is_none() {
            anyhow::bail!(
                "Missing oracle signing key: set ORACLE_SIGNING_KEY_HEX or ORACLE_SIGNING_KEY_PATH"
            );
        }

        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_env(dev_accept_all: &str) -> Vec<(String, String)> {
        vec![
            (
                "ORACLE_SERVER_BIND_ADDRESS".to_owned(),
                "127.0.0.1:0".to_owned(),
            ),
            ("ORACLE_POLICIES_DIR".to_owned(), "./policies".to_owned()),
            (
                "ORACLE_SIGNING_KEY_HEX".to_owned(),
                "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            ),
            (
                "ORACLE_DEV_ACCEPT_ALL".to_owned(),
                dev_accept_all.to_owned(),
            ),
        ]
    }

    #[test]
    fn parses_oracle_dev_accept_all_one() {
        let cfg: Config = envy::from_iter(minimal_env("1")).unwrap();
        assert!(cfg.oracle_dev_accept_all);
    }

    #[test]
    fn parses_oracle_dev_accept_all_true() {
        let cfg: Config = envy::from_iter(minimal_env("true")).unwrap();
        assert!(cfg.oracle_dev_accept_all);
    }

    #[test]
    fn parses_oracle_dev_accept_all_zero() {
        let cfg: Config = envy::from_iter(minimal_env("0")).unwrap();
        assert!(!cfg.oracle_dev_accept_all);
    }

    #[test]
    fn parses_oracle_dev_accept_all_false() {
        let cfg: Config = envy::from_iter(minimal_env("false")).unwrap();
        assert!(!cfg.oracle_dev_accept_all);
    }
}
