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

    /// Wallet private key as hex string (env: WALLET_PRIVATE_KEY, required)
    /// No files needed! Just provide the private key hex string.
    #[validate(length(min = 1))]
    pub wallet_private_key: String,

    /// Sovereign SDK rollup RPC endpoint (env: ROLLUP_RPC_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub rollup_rpc_url: Url,

    /// Verifier service URL for midnight-privacy transactions (env: VERIFIER_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub verifier_url: Url,

    /// Indexer service URL for querying transaction history (env: INDEXER_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub indexer_url: Url,

    /// Path to ZK circuit WASM program (env: ZK_PROGRAM_PATH, required)
    #[validate(custom(function = "validate_file_exists"))]
    pub ligero_program_path: PathBuf,

    /// Path to Ligero prover binary (env: LIGERO_PROVER_BINARY_PATH, required)
    #[validate(custom(function = "validate_file_exists"))]
    pub ligero_prover_binary_path: PathBuf,

    /// Path to Ligero shader directory (env: LIGERO_SHADER_PATH, required)
    #[validate(custom(function = "validate_file_exists"))]
    pub ligero_shader_path: PathBuf,

    /// Authority Full Viewing Key (FVK) for decrypting privacy pool notes (env: AUTHORITY_FVK, optional)
    /// 32-byte hex string with or without 0x prefix
    #[serde(default)]
    pub authority_fvk: Option<String>,

    /// Privacy pool spending secret key for deriving recipient addresses and spending notes (env: PRIVPOOL_SPEND_KEY, required)
    /// 32-byte hex string with or without 0x prefix, or bech32m privacy address (e.g., "privpool1...")
    /// This is REQUIRED to start the MCP server - deposits can only be made to your own privacy address
    #[validate(length(min = 1))]
    pub privpool_spend_key: String,
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
