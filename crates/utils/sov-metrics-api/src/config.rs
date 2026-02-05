use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

const DEFAULT_RETENTION_SECS: u64 = 5 * 24 * 60 * 60;
const DEFAULT_PEAK_TPS_MULTIPLIER: f64 = 1.0;

#[derive(Clone, Debug)]
pub struct Config {
    pub da_connection_string: String,
    pub indexer_db_connection_string: String,
    pub bind_addr: SocketAddr,
    pub tsink_data_path: PathBuf,
    pub tsink_retention_secs: u64,
    /// Multiplier applied to PeakTPS metric output on EMA endpoints and /tps/peak.
    pub peak_tps_multiplier: f64,
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

        let peak_tps_multiplier = match env::var("PEAK_TPS_MULTIPLIER") {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    DEFAULT_PEAK_TPS_MULTIPLIER
                } else {
                    let parsed = trimmed
                        .parse::<f64>()
                        .map_err(|_| anyhow!("PEAK_TPS_MULTIPLIER must be a valid number"))?;
                    if parsed < 0.0 {
                        return Err(anyhow!("PEAK_TPS_MULTIPLIER must be >= 0"));
                    }
                    parsed
                }
            }
            Err(_) => DEFAULT_PEAK_TPS_MULTIPLIER,
        };

        Ok(Self {
            da_connection_string,
            indexer_db_connection_string,
            bind_addr,
            tsink_data_path: PathBuf::from(tsink_data_path),
            tsink_retention_secs,
            peak_tps_multiplier,
        })
    }
}
