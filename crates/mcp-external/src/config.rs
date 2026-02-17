use anyhow::{Context, Result};
use serde::Deserialize;
use url::Url;
use validator::Validate;

fn default_nightstream_program() -> String {
    "note_spend_guest".to_string()
}

fn default_nightstream_proof_service_url() -> Url {
    Url::parse("http://127.0.0.1:8080").expect("default proof service URL is valid")
}

#[derive(Debug, Clone, Deserialize, Validate)]
pub struct Config {
    /// Server bind address (env: MCP_SERVER_BIND_ADDRESS, default: "127.0.0.1:3000")
    #[serde(default = "default_server_bind_address")]
    #[validate(length(min = 1))]
    pub mcp_server_bind_address: String,

    /// Start with a new randomly generated wallet and privacy key (env: START_WITH_NEW_WALLET, optional)
    /// When true, WALLET_PRIVATE_KEY and PRIVPOOL_SPEND_KEY are ignored.
    #[serde(default)]
    pub start_with_new_wallet: bool,

    /// Wallet private key as hex string (env: WALLET_PRIVATE_KEY, required unless START_WITH_NEW_WALLET=true)
    /// No files needed! Just provide the private key hex string.
    #[serde(default)]
    pub wallet_private_key: Option<String>,

    /// Admin wallet private key used for auto-funding newly created wallets (env: ADMIN_WALLET_PRIVATE_KEY, optional)
    /// This key remains immutable and is not affected by restoreWallet.
    #[serde(default)]
    pub admin_wallet_private_key: Option<String>,

    /// Sovereign SDK rollup RPC endpoint (env: ROLLUP_RPC_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub rollup_rpc_url: Url,

    /// Verifier service URL for midnight-privacy transactions (env: VERIFIER_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub verifier_url: Url,

    /// Indexer service URL for querying transaction history (env: INDEXER_URL, required)
    #[validate(custom(function = "validate_http_url"))]
    pub indexer_url: Url,

    /// Nightstream circuit/program identifier (env: NIGHTSTREAM_PROGRAM_PATH, optional).
    ///
    /// Accepts either a circuit name (e.g. `note_spend_guest`) or a full path.
    /// Defaults to `note_spend_guest`.
    #[serde(default = "default_nightstream_program", alias = "ZK_PROGRAM_PATH", alias = "ligero_program_path")]
    #[validate(custom(function = "validate_nightstream_program"))]
    pub nightstream_program_path: String,

    /// Nightstream proof service URL (env: NIGHTSTREAM_PROOF_SERVICE_URL, alias: LIGERO_PROOF_SERVICE_URL).
    #[serde(default = "default_nightstream_proof_service_url", alias = "ligero_proof_service_url")]
    #[validate(custom(function = "validate_http_url"))]
    pub nightstream_proof_service_url: Url,

    /// Privacy pool spending secret key for deriving recipient addresses and spending notes (env: PRIVPOOL_SPEND_KEY, required unless START_WITH_NEW_WALLET=true)
    /// 32-byte hex string with or without 0x prefix, or bech32m privacy address (e.g., "privpool1...")
    /// Required to start the MCP server unless START_WITH_NEW_WALLET=true.
    #[serde(default)]
    pub privpool_spend_key: Option<String>,

    /// Optional amount to auto-fund a new wallet (env: AUTO_FUND_DEPOSIT_AMOUNT, optional; alias: STARTUP_DEPOSIT_AMOUNT).
    /// Requires ADMIN_WALLET_PRIVATE_KEY to be set.
    #[serde(default, alias = "AUTO_FUND_DEPOSIT_AMOUNT")]
    pub auto_fund_deposit_amount: Option<String>,

    /// Optional gas reserve to add when auto-funding a new wallet (env: AUTO_FUND_GAS_RESERVE, optional).
    /// This is added to the deposit amount to cover future transaction fees.
    #[serde(default)]
    pub auto_fund_gas_reserve: Option<String>,

    /// Optional JSONL file with prefunded wallet credentials (env: PREFUNDED_WALLETS_FILE, optional).
    ///
    /// When set, `createWallet` will claim an unused prefunded wallet from the indexer instead of
    /// generating and funding a wallet on-demand.
    ///
    /// The file is expected to contain one JSON object per line, including:
    /// - wallet_private_key_hex (32-byte hex)
    /// - privacy_spend_key_hex (32-byte hex)
    /// - wallet_address (sov1...)
    /// - privacy_address (privpool1...)
    #[serde(default)]
    pub prefunded_wallets_file: Option<String>,

    /// Bearer token for the `/authority` HTTP endpoints (env: MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN).
    ///
    /// Uses the same token as the FVK service admin. If set, POST endpoints such as
    /// `/authority/freeze` and `/authority/thaw` require `Authorization: Bearer <token>`.
    #[serde(default)]
    pub midnight_fvk_service_admin_token: Option<String>,

    /// Optional base URL for `sov-metrics-api` (env: METRICS_API_URL, optional).
    ///
    /// When set, `/authority/tps` is backed by `GET <METRICS_API_URL>/tps?window_seconds=...`.
    #[serde(default)]
    #[validate(custom(function = "validate_http_url"))]
    pub metrics_api_url: Option<Url>,

    /// Optional Postgres connection string for MCP session persistence (env: MCP_SESSION_DB_URL).
    #[serde(default)]
    pub mcp_session_db_url: Option<String>,

    /// Optional 32-byte encryption key (hex or base64) for MCP session persistence (env: MCP_SESSION_DB_ENCRYPTION_KEY).
    #[serde(default)]
    pub mcp_session_db_encryption_key: Option<String>,

    /// Automatically send MCP `notifications/initialized` and bootstrap persisted sessions (env: MCP_AUTO_INITIALIZE_SESSIONS).
    #[serde(default)]
    pub mcp_auto_initialize_sessions: bool,

    /// Automatically create a wallet for auto-bootstrapped/new sessions (env: MCP_AUTO_CREATE_WALLET).
    /// Requires MCP_AUTO_INITIALIZE_SESSIONS=true.
    #[serde(default)]
    pub mcp_auto_create_wallet: bool,
}

fn default_server_bind_address() -> String {
    "127.0.0.1:3000".into()
}

fn validate_nightstream_program(program: &String) -> Result<(), validator::ValidationError> {
    let program = program.trim();
    if program.is_empty() {
        return Err(validator::ValidationError::new("empty_program")
            .with_message("NIGHTSTREAM_PROGRAM_PATH must be set (circuit name or path)".into()));
    }

    if std::path::Path::new(program).exists() {
        return Ok(());
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

        let mut cfg: Self =
            envy::from_env().context("Failed to load configuration from environment variables")?;

        cfg.wallet_private_key = cfg
            .wallet_private_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        cfg.privpool_spend_key = cfg
            .privpool_spend_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        cfg.midnight_fvk_service_admin_token = cfg
            .midnight_fvk_service_admin_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        cfg.mcp_session_db_url = cfg
            .mcp_session_db_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        cfg.mcp_session_db_encryption_key = cfg
            .mcp_session_db_encryption_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        cfg.prefunded_wallets_file = cfg
            .prefunded_wallets_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

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
