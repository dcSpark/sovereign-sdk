use anyhow::{Context, Result};
use serde::Deserialize;
use validator::Validate;

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

        Ok(cfg)
    }
}
