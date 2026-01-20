use std::env;
use std::net::SocketAddr;

use anyhow::{anyhow, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub da_connection_string: String,
    pub bind_addr: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let da_connection_string = env::var("DA_CONNECTION_STRING")
            .map_err(|_| anyhow!("DA_CONNECTION_STRING env var is required"))?;
        if da_connection_string.trim().is_empty() {
            return Err(anyhow!("DA_CONNECTION_STRING env var is empty"));
        }

        let bind_addr = env::var("METRICS_API_BIND")
            .unwrap_or_else(|_| "0.0.0.0:13200".to_string());
        if bind_addr.trim().is_empty() {
            return Err(anyhow!("METRICS_API_BIND env var is empty"));
        }
        let bind_addr: SocketAddr = bind_addr
            .parse()
            .map_err(|_| anyhow!("METRICS_API_BIND must be a valid host:port"))?;

        Ok(Self {
            da_connection_string,
            bind_addr,
        })
    }
}
