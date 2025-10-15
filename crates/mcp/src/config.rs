use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use url::Url;
use validator::Validate;

#[derive(Debug, Clone, Deserialize, Validate)]
pub struct Config {
    /// Server bind address (env: MCP_SERVER_BIND_ADDRESS, default: "127.0.0.1:3000")
    #[serde(default = "default_server_bind_address")]
    #[validate(length(min = 1))]
    pub mcp_server_bind_address: String,

    /// Path to wallet state JSON file (env: WALLET_PATH, required)
    #[validate(custom(function = "validate_file_exists"))]
    pub wallet_path: PathBuf,

    /// Sovereign SDK rollup RPC endpoint (env: ROLLUP_RPC_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub rollup_rpc_url: Url,

    /// Path to ZK circuit WASM program (env: ZK_PROGRAM_PATH, required)
    #[validate(custom(function = "validate_file_exists"))]
    pub zk_program_path: PathBuf,
}

fn default_server_bind_address() -> String {
    "127.0.0.1:3000".into()
}

fn validate_file_exists(path: &PathBuf) -> Result<(), validator::ValidationError> {
    if !path.exists() {
        return Err(validator::ValidationError::new("file_not_found")
            .with_message(format!("File does not exist: {}", path.display()).into()));
    }
    Ok(())
}

fn validate_http_url(url: &Url) -> Result<(), validator::ValidationError> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(validator::ValidationError::new("invalid_scheme")
            .with_message(format!("URL must use http/https, got: {}", scheme).into())),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let cfg: Self =
            envy::from_env().context("Failed to load configuration from environment variables")?;

        // Validate all fields using the validator derive macro
        if let Err(errors) = cfg.validate() {
            eprintln!("\nConfiguration validation failed:");
            for (field, field_errors) in errors.field_errors() {
                for error in field_errors {
                    let message = error
                        .message
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| format!("Validation error: {}", error.code));
                    eprintln!("  • {}: {}", field, message);
                }
            }
            eprintln!();
            anyhow::bail!("Configuration validation failed");
        }

        Ok(cfg)
    }
}
