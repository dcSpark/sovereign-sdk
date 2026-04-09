use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use serde::{Deserialize, Serialize};
use sov_address::MultiAddressEvm;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_nightstream_adapter::Nightstream;

use crate::privacy_key::PrivacyKey;
use crate::wallet::WalletContext;

const DOMAIN: [u8; 32] = [1u8; 32];

type McpSpec = ConfigurableSpec<MockDaSpec, Nightstream, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;
type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

#[derive(Debug, Clone)]
pub struct PrefundedWalletCredentials {
    pub wallet_private_key_hex: String,
    pub privacy_spend_key_hex: String,
    pub wallet_address: String,
    pub privacy_address: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrefundedWalletImportItem {
    pub wallet_address: String,
    pub privacy_address: String,
}

#[derive(Debug)]
pub struct PrefundedWalletStore {
    source_path: PathBuf,
    by_wallet_address: HashMap<String, PrefundedWalletCredentials>,
    import_items: Vec<PrefundedWalletImportItem>,
}

#[derive(Debug, Deserialize)]
struct JsonlEntry {
    wallet_private_key_hex: String,
    privacy_spend_key_hex: String,
    #[serde(default)]
    wallet_address: Option<String>,
    #[serde(default)]
    privacy_address: Option<String>,
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

impl PrefundedWalletStore {
    pub fn load_jsonl(path: impl AsRef<Path>) -> Result<Self> {
        let source_path = path.as_ref().to_path_buf();
        let file = File::open(&source_path)
            .with_context(|| format!("Failed to open prefunded wallets file: {source_path:?}"))?;
        let reader = BufReader::new(file);

        let mut by_wallet_address = HashMap::<String, PrefundedWalletCredentials>::new();

        for (idx, line) in reader.lines().enumerate() {
            let line_no = idx + 1;
            let line = line.with_context(|| {
                format!("Failed to read prefunded wallets file {source_path:?} at line {line_no}")
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let entry: JsonlEntry = serde_json::from_str(trimmed).with_context(|| {
                format!("Invalid JSON in prefunded wallets file {source_path:?} at line {line_no}")
            })?;

            let wallet_private_key_hex =
                normalize_hex_32("wallet_private_key_hex", &entry.wallet_private_key_hex)
                    .with_context(|| {
                        format!(
                            "Invalid wallet_private_key_hex in {source_path:?} at line {line_no}"
                        )
                    })?;
            let privacy_spend_key_hex =
                normalize_hex_32("privacy_spend_key_hex", &entry.privacy_spend_key_hex)
                    .with_context(|| {
                        format!(
                            "Invalid privacy_spend_key_hex in {source_path:?} at line {line_no}"
                        )
                    })?;

            let wallet_ctx = McpWalletContext::from_private_key_hex(&wallet_private_key_hex)
                .with_context(|| {
                    format!(
                        "Failed to derive wallet address from wallet_private_key_hex in {source_path:?} at line {line_no}"
                    )
                })?;
            let derived_wallet_address = wallet_ctx.get_address().to_string();
            let wallet_address = entry
                .wallet_address
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(derived_wallet_address.as_str())
                .to_string();
            anyhow::ensure!(
                wallet_address == derived_wallet_address,
                "wallet_address mismatch in {source_path:?} at line {line_no}: derived {derived_wallet_address}, got {wallet_address}"
            );

            let privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex).with_context(|| {
                format!(
                    "Failed to derive privacy address from privacy_spend_key_hex in {source_path:?} at line {line_no}"
                )
            })?;
            let derived_privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();
            let privacy_address = entry
                .privacy_address
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(derived_privacy_address.as_str())
                .to_string();
            anyhow::ensure!(
                privacy_address == derived_privacy_address,
                "privacy_address mismatch in {source_path:?} at line {line_no}: derived {derived_privacy_address}, got {privacy_address}"
            );

            if by_wallet_address.contains_key(&wallet_address) {
                anyhow::bail!(
                    "Duplicate wallet_address {wallet_address} in {source_path:?} at line {line_no}"
                );
            }

            by_wallet_address.insert(
                wallet_address.clone(),
                PrefundedWalletCredentials {
                    wallet_private_key_hex,
                    privacy_spend_key_hex,
                    wallet_address,
                    privacy_address,
                },
            );
        }

        let mut import_items = Vec::with_capacity(by_wallet_address.len());
        for creds in by_wallet_address.values() {
            import_items.push(PrefundedWalletImportItem {
                wallet_address: creds.wallet_address.clone(),
                privacy_address: creds.privacy_address.clone(),
            });
        }

        Ok(Self {
            source_path,
            by_wallet_address,
            import_items,
        })
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn len(&self) -> usize {
        self.by_wallet_address.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_wallet_address.is_empty()
    }

    pub fn get(&self, wallet_address: &str) -> Option<&PrefundedWalletCredentials> {
        self.by_wallet_address.get(wallet_address)
    }

    pub fn import_items(&self) -> &[PrefundedWalletImportItem] {
        &self.import_items
    }
}
