use std::net::SocketAddr;
use anyhow::{anyhow, Result};
use fake_attestation_server::ServerError;

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = "127.0.0.1:8000".to_string().parse().unwrap();
    fake_attestation_server::serve(addr).await?;
    Ok(())
}
