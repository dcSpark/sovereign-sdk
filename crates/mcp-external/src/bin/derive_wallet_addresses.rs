use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::wallet::WalletContext;
use serde::Serialize;
use sov_address::MultiAddressEvm;
use sov_ligero_adapter::Ligero as LigeroAdapter;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;

const DOMAIN: [u8; 32] = [1u8; 32];

type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;
type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

#[derive(Serialize)]
struct DerivedAddresses {
    wallet_private_key_hex: String,
    privacy_spend_key_hex: String,
    wallet_address: String,
    privacy_address: String,
    privacy_key_reused_from_wallet_key: bool,
}

fn usage() -> &'static str {
    "Derive wallet and privacy addresses from private key material.

Usage:
  cargo run -p mcp-external --bin derive_wallet_addresses -- --wallet-private-key <HEX> [--privacy-spend-key <HEX>]
  cargo run -p mcp-external --bin derive_wallet_addresses -- <wallet_private_key_hex> [privacy_spend_key_hex]

Notes:
  - Keys must be 32-byte hex (with or without 0x prefix).
  - If privacy_spend_key_hex is omitted, wallet_private_key_hex is reused to derive privacy_address."
}

fn normalize_hex_32(label: &str, value: &str) -> Result<String> {
    let s = value.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).with_context(|| format!("Invalid hex for {label}"))?;
    anyhow::ensure!(
        bytes.len() == 32,
        "{label} must be 32 bytes (got {} bytes)",
        bytes.len()
    );
    Ok(hex::encode(bytes))
}

fn parse_args() -> Result<(String, Option<String>)> {
    let mut wallet_private_key: Option<String> = None;
    let mut privacy_spend_key: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "--wallet-private-key" => {
                let value = args
                    .next()
                    .context("Missing value for --wallet-private-key")?;
                wallet_private_key = Some(value);
            }
            "--privacy-spend-key" => {
                let value = args
                    .next()
                    .context("Missing value for --privacy-spend-key")?;
                privacy_spend_key = Some(value);
            }
            _ if arg.starts_with("--") => {
                anyhow::bail!("Unknown argument: {arg}\n\n{}", usage());
            }
            _ => {
                if wallet_private_key.is_none() {
                    wallet_private_key = Some(arg);
                } else if privacy_spend_key.is_none() {
                    privacy_spend_key = Some(arg);
                } else {
                    anyhow::bail!("Unexpected extra positional argument: {arg}\n\n{}", usage());
                }
            }
        }
    }

    let wallet_private_key =
        wallet_private_key.context(format!("Missing wallet private key.\n\n{}", usage()))?;

    Ok((wallet_private_key, privacy_spend_key))
}

fn main() -> Result<()> {
    let (wallet_private_key_raw, privacy_spend_key_raw) = parse_args()?;
    let wallet_private_key_hex = normalize_hex_32("wallet_private_key", &wallet_private_key_raw)?;

    let (privacy_spend_key_hex, reused_wallet_key) = if let Some(raw) = privacy_spend_key_raw {
        (normalize_hex_32("privacy_spend_key", &raw)?, false)
    } else {
        (wallet_private_key_hex.clone(), true)
    };

    let wallet = McpWalletContext::from_private_key_hex(&wallet_private_key_hex)
        .context("Failed to derive wallet address from wallet private key")?;
    let privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex)
        .context("Failed to derive privacy address from privacy spend key")?;

    let derived = DerivedAddresses {
        wallet_private_key_hex,
        privacy_spend_key_hex,
        wallet_address: wallet.get_address().to_string(),
        privacy_address: privacy_key.privacy_address(&DOMAIN).to_string(),
        privacy_key_reused_from_wallet_key: reused_wallet_key,
    };

    println!("{}", serde_json::to_string_pretty(&derived)?);
    Ok(())
}
