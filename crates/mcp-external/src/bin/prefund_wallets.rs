use std::env;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use futures::stream::{self, StreamExt};
use mcp_external::operations;
use mcp_external::privacy_key::PrivacyKey;
use mcp_external::provider::Provider;
use mcp_external::wallet::WalletContext;
use rand::RngCore;
use sov_address::MultiAddressEvm;
use sov_bank::{config_gas_token_id, TokenId};
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::{Amount, Spec};
use sov_nightstream_adapter::Nightstream as NightstreamAdapter;

const DOMAIN: [u8; 32] = [1u8; 32];

type McpSpec = ConfigurableSpec<MockDaSpec, NightstreamAdapter, MockZkvm, MultiAddressEvm, Native>;
type McpRuntime = Runtime<McpSpec>;
type McpWalletContext = WalletContext<McpRuntime, McpSpec>;

fn env_required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is required"))
}

fn env_u128_optional(name: &str) -> Option<u128> {
    env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u128>().ok())
}

fn env_usize_optional(name: &str) -> Option<usize> {
    env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
}

fn env_u64_optional(name: &str) -> Option<u64> {
    env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

fn env_bool_optional(name: &str) -> Option<bool> {
    env::var(name)
        .ok()
        .and_then(|raw| match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "y" | "on" => Some(true),
            "0" | "false" | "no" | "n" | "off" => Some(false),
            _ => None,
        })
}

fn parse_hex_32(label: &str, value: &str) -> Result<String> {
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

async fn wait_for_sequencer_acceptance(provider: &Provider, tx_hash: &str) -> Result<()> {
    use sov_api_spec::types::TxReceiptResult;

    let max_wait = env_u64_optional("PREFUND_TX_MAX_WAIT_SECS").unwrap_or(300);
    let poll_interval = env_u64_optional("PREFUND_TX_POLL_INTERVAL_SECS").unwrap_or(2);

    let max_wait = Duration::from_secs(max_wait);
    let poll_interval = Duration::from_secs(poll_interval);
    let started = Instant::now();

    loop {
        match provider.get_sequencer_tx(tx_hash).await {
            Ok(Some(tx)) => match &tx.receipt.result {
                TxReceiptResult::Successful => return Ok(()),
                TxReceiptResult::Reverted | TxReceiptResult::Skipped => {
                    anyhow::bail!(
                        "Funding tx failed in sequencer: receipt={:?}",
                        tx.receipt.result
                    );
                }
            },
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("Failed to query sequencer for funding tx receipt: {}", e);
            }
        }

        if started.elapsed() >= max_wait {
            anyhow::bail!("Timed out waiting for funding tx to appear in sequencer");
        }

        tokio::time::sleep(poll_interval).await;
    }
}

async fn wait_for_balance(
    provider: &Provider,
    wallet_address: &<McpSpec as Spec>::Address,
    token_id: &TokenId,
    min_balance: u128,
) -> Result<u128> {
    let max_wait = env_u64_optional("PREFUND_BALANCE_MAX_WAIT_SECS").unwrap_or(30);
    let poll_interval = env_u64_optional("PREFUND_BALANCE_POLL_INTERVAL_SECS").unwrap_or(2);

    let max_wait = Duration::from_secs(max_wait);
    let poll_interval = Duration::from_secs(poll_interval);
    let started = Instant::now();

    loop {
        match provider
            .get_balance::<McpSpec>(wallet_address, token_id)
            .await
        {
            Ok(balance) => {
                let bal_u128: u128 = balance.0;
                if bal_u128 >= min_balance {
                    return Ok(bal_u128);
                }
                tracing::info!("Waiting for funding: {} / {}", bal_u128, min_balance);
            }
            Err(e) => tracing::warn!("Failed to query balance while waiting for funding: {}", e),
        }

        if started.elapsed() >= max_wait {
            anyhow::bail!("Timed out waiting for wallet to be funded");
        }

        tokio::time::sleep(poll_interval).await;
    }
}

fn generate_key_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();

    let rpc_url = env_required("ROLLUP_RPC_URL")?;
    let verifier_url = env_required("VERIFIER_URL")?;
    let indexer_url = env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:13100".into());
    let admin_wallet_private_key = env_required("ADMIN_WALLET_PRIVATE_KEY")?;

    let prefund_count = env_usize_optional("PREFUND_COUNT").unwrap_or(1000);
    let prefund_concurrency_cfg = env_usize_optional("PREFUND_CONCURRENCY").unwrap_or(10);
    let prefund_concurrency = prefund_concurrency_cfg.clamp(1, prefund_count.max(1));
    let output_file = env_required("PREFUND_OUTPUT_FILE")?;
    let append = env_bool_optional("PREFUND_APPEND").unwrap_or(true);

    let deposit_amount = env_u128_optional("PREFUND_DEPOSIT_AMOUNT")
        .or_else(|| env_u128_optional("AUTO_FUND_DEPOSIT_AMOUNT"))
        .context("PREFUND_DEPOSIT_AMOUNT or AUTO_FUND_DEPOSIT_AMOUNT is required")?;
    let gas_reserve_cfg = env_u128_optional("PREFUND_GAS_RESERVE")
        .or_else(|| env_u128_optional("AUTO_FUND_GAS_RESERVE"))
        .unwrap_or(1_000_000);

    let admin_wallet_private_key =
        parse_hex_32("ADMIN_WALLET_PRIVATE_KEY", &admin_wallet_private_key)?;

    let provider = Provider::new(&rpc_url, &verifier_url, &indexer_url).await?;
    let admin_wallet_ctx = McpWalletContext::from_private_key_hex(&admin_wallet_private_key)
        .context("Admin wallet")?;
    tracing::info!(
        "Funding from admin wallet: {}",
        admin_wallet_ctx.get_address()
    );

    let min_gas_reserve = operations::DEFAULT_MAX_FEE;
    let gas_reserve = gas_reserve_cfg.max(min_gas_reserve);
    let l2_funding_amount = deposit_amount + gas_reserve;

    let gas_token_id = match provider.get_gas_token_id().await {
        Ok(token_id) => {
            let configured = config_gas_token_id();
            if token_id != configured {
                tracing::warn!(
                    "Gas token mismatch: chain {}, configured {}",
                    token_id,
                    configured
                );
            }
            token_id
        }
        Err(e) => {
            tracing::warn!(
                "Failed to fetch gas token id from rollup: {}. Falling back to configured gas token.",
                e
            );
            config_gas_token_id()
        }
    };

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&output_file)
        .with_context(|| format!("Failed to open PREFUND_OUTPUT_FILE: {}", output_file))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&output_file, perms)?;
    }

    let mut writer = BufWriter::new(file);

    tracing::info!(
        "Prefunding {} wallets: deposit_amount={}, gas_reserve={}, l2_funding_amount={}, out={}, concurrency={}",
        prefund_count,
        deposit_amount,
        gas_reserve,
        l2_funding_amount,
        output_file,
        prefund_concurrency
    );

    let mut completed = 0usize;

    let mut tasks = stream::iter(0..prefund_count)
        .map(|i| {
            let provider = provider.clone();
            let admin_wallet_ctx = admin_wallet_ctx.clone();
            let gas_token_id = gas_token_id.clone();
            async move {
                let wallet_private_key_hex = generate_key_hex();
                let privacy_spend_key_hex = generate_key_hex();

                let wallet_ctx = McpWalletContext::from_private_key_hex(&wallet_private_key_hex)
                    .with_context(|| format!("Failed to create wallet context (idx={})", i))?;
                let wallet_address = wallet_ctx.get_address().to_string();

                let privacy_key = PrivacyKey::from_hex(&privacy_spend_key_hex)
                    .with_context(|| format!("Failed to create privacy key (idx={})", i))?;
                let privacy_address = privacy_key.privacy_address(&DOMAIN).to_string();

                tracing::info!(
                    "[{}/{}] Funding wallet {} (privacy {})",
                    i + 1,
                    prefund_count,
                    wallet_address,
                    privacy_address
                );

                let send_res = operations::send_funds(
                    &provider,
                    &admin_wallet_ctx,
                    &wallet_address,
                    &gas_token_id,
                    Amount::from(l2_funding_amount),
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed to send funds to wallet {} (idx={})",
                        wallet_address, i
                    )
                })?;
                let funding_tx_hash = send_res.tx_hash.trim().to_string();
                anyhow::ensure!(
                    !funding_tx_hash.is_empty(),
                    "Funding tx hash is empty (wallet {}, idx={})",
                    wallet_address,
                    i
                );

                wait_for_sequencer_acceptance(&provider, &funding_tx_hash)
                    .await
                    .with_context(|| {
                        format!(
                            "Funding tx not accepted by sequencer (tx={}, wallet={}, idx={})",
                            funding_tx_hash, wallet_address, i
                        )
                    })?;

                let wallet_address_parsed = wallet_ctx.get_address();
                let _ = wait_for_balance(
                    &provider,
                    &wallet_address_parsed,
                    &gas_token_id,
                    l2_funding_amount,
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed waiting for L2 funding (wallet={}, idx={})",
                        wallet_address, i
                    )
                })?;

                let deposit_res =
                    operations::deposit(&provider, &wallet_ctx, deposit_amount, &privacy_key)
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to deposit to privacy pool (wallet={}, idx={})",
                                wallet_address, i
                            )
                        })?;
                let deposit_tx_hash = deposit_res.tx_hash.trim().to_string();

                Ok::<_, anyhow::Error>(serde_json::json!({
                    "wallet_private_key_hex": wallet_private_key_hex,
                    "privacy_spend_key_hex": privacy_spend_key_hex,
                    "wallet_address": wallet_address,
                    "privacy_address": privacy_address,
                    "funding_tx_hash": funding_tx_hash,
                    "deposit_tx_hash": deposit_tx_hash,
                }))
            }
        })
        .buffer_unordered(prefund_concurrency);

    while let Some(res) = tasks.next().await {
        let json = res?;
        completed += 1;
        writeln!(&mut writer, "{}", json.to_string())?;
        writer.flush()?;
        if completed % 10 == 0 || completed == prefund_count {
            tracing::info!(
                "[{}/{}] Prefunded wallets written",
                completed,
                prefund_count
            );
        }
    }

    Ok(())
}
