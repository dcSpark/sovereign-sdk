use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

const DEFAULT_RETENTION_SECS: u64 = 5 * 24 * 60 * 60;
const DEFAULT_TPS_ROUNDING_DECIMALS: u32 = 2;
const DEFAULT_POSTGRES_MAX_CONNECTIONS: u32 = 10;
const DEFAULT_POSTGRES_MIN_CONNECTIONS: u32 = 0;
const DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECS: u64 = 30;
const DEFAULT_POSTGRES_IDLE_TIMEOUT_SECS: u64 = 10 * 60;
const DEFAULT_POSTGRES_MAX_LIFETIME_SECS: u64 = 30 * 60;

#[derive(Clone, Debug)]
pub struct Config {
    pub da_connection_string: String,
    pub indexer_db_connection_string: String,
    pub ledger_api_base_url: String,
    pub bind_addr: SocketAddr,
    pub tsink_data_path: PathBuf,
    pub tsink_retention_secs: u64,
    /// Number of decimal places for TPS, PeakTPS, and TokensPerSecond on EMA endpoints.
    pub tps_rounding_decimals: u32,
    pub da_postgres_max_connections: u32,
    pub da_postgres_min_connections: u32,
    pub indexer_postgres_max_connections: u32,
    pub indexer_postgres_min_connections: u32,
    pub postgres_acquire_timeout_secs: u64,
    pub postgres_idle_timeout_secs: u64,
    pub postgres_max_lifetime_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let da_connection_string = env::var("DA_CONNECTION_STRING")
            .map_err(|_| anyhow!("DA_CONNECTION_STRING env var is required"))?;
        if da_connection_string.trim().is_empty() {
            return Err(anyhow!("DA_CONNECTION_STRING env var is empty"));
        }

        let indexer_db_connection_string = env::var("INDEXER_DB_CONNECTION_STRING")
            .or_else(|_| env::var("INDEX_DB"))
            .map_err(|_| {
                anyhow!("INDEXER_DB_CONNECTION_STRING (or INDEX_DB) env var is required")
            })?;
        if indexer_db_connection_string.trim().is_empty() {
            return Err(anyhow!(
                "INDEXER_DB_CONNECTION_STRING (or INDEX_DB) env var is empty"
            ));
        }

        let ledger_api_base_url = env::var("LEDGER_API_URL")
            .or_else(|_| env::var("ROLLUP_RPC_URL"))
            .or_else(|_| env::var("NODE_API_URL"))
            .unwrap_or_else(|_| "http://127.0.0.1:12346".to_string());
        let ledger_api_base_url = ledger_api_base_url.trim().trim_end_matches('/').to_string();
        if ledger_api_base_url.is_empty() {
            return Err(anyhow!(
                "LEDGER_API_URL (or ROLLUP_RPC_URL / NODE_API_URL) env var is empty"
            ));
        }
        reqwest::Url::parse(&ledger_api_base_url)
            .map_err(|_| anyhow!("LEDGER_API_URL must be a valid absolute URL"))?;

        let bind_addr =
            env::var("METRICS_API_BIND").unwrap_or_else(|_| "0.0.0.0:13200".to_string());
        if bind_addr.trim().is_empty() {
            return Err(anyhow!("METRICS_API_BIND env var is empty"));
        }
        let bind_addr: SocketAddr = bind_addr
            .parse()
            .map_err(|_| anyhow!("METRICS_API_BIND must be a valid host:port"))?;

        let tsink_data_path = env::var("TSINK_DATA_PATH")
            .map_err(|_| anyhow!("TSINK_DATA_PATH env var is required"))?;
        if tsink_data_path.trim().is_empty() {
            return Err(anyhow!("TSINK_DATA_PATH env var is empty"));
        }

        let tsink_retention_secs = match env::var("TSINK_RETENTION_SECONDS") {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(anyhow!("TSINK_RETENTION_SECONDS env var is empty"));
                }
                let parsed = trimmed
                    .parse::<u64>()
                    .map_err(|_| anyhow!("TSINK_RETENTION_SECONDS must be a positive integer"))?;
                if parsed == 0 {
                    return Err(anyhow!("TSINK_RETENTION_SECONDS must be > 0"));
                }
                parsed
            }
            Err(_) => DEFAULT_RETENTION_SECS,
        };

        let tps_rounding_decimals =
            env_u32_or_default("TPS_ROUNDING_DECIMALS", DEFAULT_TPS_ROUNDING_DECIMALS)?;

        let da_postgres_max_connections = env_u32_or_default(
            "SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS",
            DEFAULT_POSTGRES_MAX_CONNECTIONS,
        )?;
        if da_postgres_max_connections == 0 {
            return Err(anyhow!(
                "SOV_METRICS_API_DA_POSTGRES_MAX_CONNECTIONS must be > 0"
            ));
        }

        let da_postgres_min_connections = env_u32_or_default(
            "SOV_METRICS_API_DA_POSTGRES_MIN_CONNECTIONS",
            DEFAULT_POSTGRES_MIN_CONNECTIONS,
        )?
        .min(da_postgres_max_connections);

        let indexer_postgres_max_connections = env_u32_or_default(
            "SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS",
            DEFAULT_POSTGRES_MAX_CONNECTIONS,
        )?;
        if indexer_postgres_max_connections == 0 {
            return Err(anyhow!(
                "SOV_METRICS_API_INDEXER_POSTGRES_MAX_CONNECTIONS must be > 0"
            ));
        }

        let indexer_postgres_min_connections = env_u32_or_default(
            "SOV_METRICS_API_INDEXER_POSTGRES_MIN_CONNECTIONS",
            DEFAULT_POSTGRES_MIN_CONNECTIONS,
        )?
        .min(indexer_postgres_max_connections);

        let postgres_acquire_timeout_secs = env_u64_or_default(
            "SOV_METRICS_API_POSTGRES_ACQUIRE_TIMEOUT_SECS",
            DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECS,
        )?;
        if postgres_acquire_timeout_secs == 0 {
            return Err(anyhow!(
                "SOV_METRICS_API_POSTGRES_ACQUIRE_TIMEOUT_SECS must be > 0"
            ));
        }

        let postgres_idle_timeout_secs = env_u64_or_default(
            "SOV_METRICS_API_POSTGRES_IDLE_TIMEOUT_SECS",
            DEFAULT_POSTGRES_IDLE_TIMEOUT_SECS,
        )?;
        if postgres_idle_timeout_secs == 0 {
            return Err(anyhow!(
                "SOV_METRICS_API_POSTGRES_IDLE_TIMEOUT_SECS must be > 0"
            ));
        }

        let postgres_max_lifetime_secs = env_u64_or_default(
            "SOV_METRICS_API_POSTGRES_MAX_LIFETIME_SECS",
            DEFAULT_POSTGRES_MAX_LIFETIME_SECS,
        )?;
        if postgres_max_lifetime_secs == 0 {
            return Err(anyhow!(
                "SOV_METRICS_API_POSTGRES_MAX_LIFETIME_SECS must be > 0"
            ));
        }

        Ok(Self {
            da_connection_string,
            indexer_db_connection_string,
            ledger_api_base_url,
            bind_addr,
            tsink_data_path: PathBuf::from(tsink_data_path),
            tsink_retention_secs,
            tps_rounding_decimals,
            da_postgres_max_connections,
            da_postgres_min_connections,
            indexer_postgres_max_connections,
            indexer_postgres_min_connections,
            postgres_acquire_timeout_secs,
            postgres_idle_timeout_secs,
            postgres_max_lifetime_secs,
        })
    }
}

fn env_u32_or_default(var_name: &str, default: u32) -> Result<u32> {
    match env::var(var_name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(anyhow!("{var_name} env var is empty"));
            }
            trimmed
                .parse::<u32>()
                .map_err(|_| anyhow!("{var_name} must be a non-negative integer"))
        }
        Err(_) => Ok(default),
    }
}

fn env_u64_or_default(var_name: &str, default: u64) -> Result<u64> {
    match env::var(var_name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(anyhow!("{var_name} env var is empty"));
            }
            trimmed
                .parse::<u64>()
                .map_err(|_| anyhow!("{var_name} must be a non-negative integer"))
        }
        Err(_) => Ok(default),
    }
}
