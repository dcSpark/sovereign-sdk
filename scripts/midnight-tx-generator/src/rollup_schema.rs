use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct SchemaResp {
    chain_hash: String,
}

pub fn fetch_rollup_chain_hash(node_url: &str) -> Result<[u8; 32]> {
    let base = node_url.trim_end_matches('/');
    let url = format!("{}/rollup/schema", base);
    let resp = reqwest::blocking::get(&url)
        .with_context(|| format!("Failed to fetch {}", url))?
        .error_for_status()
        .with_context(|| format!("Non-success response from {}", url))?;
    let schema: SchemaResp = resp
        .json()
        .context("Failed to parse /rollup/schema response")?;

    let chain_hash_hex = schema.chain_hash.trim_start_matches("0x");
    let chain_hash_vec = hex::decode(chain_hash_hex).with_context(|| {
        format!(
            "Invalid chain_hash returned by node: {}",
            schema.chain_hash
        )
    })?;
    if chain_hash_vec.len() != 32 {
        return Err(anyhow::anyhow!(
            "chain_hash must be 32 bytes (got {})",
            chain_hash_vec.len()
        ));
    }
    let mut chain_hash = [0u8; 32];
    chain_hash.copy_from_slice(&chain_hash_vec);
    Ok(chain_hash)
}
