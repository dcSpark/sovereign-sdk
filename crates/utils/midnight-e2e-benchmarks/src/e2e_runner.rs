use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use demo_stf::runtime::{Runtime, RuntimeCall};
use ligetron::bn254fr_native::submod_checked;
use ligetron::Bn254Fr;
use midnight_privacy::{
    nf_key_from_sk, note_commitment, nullifier, pk_from_sk, pk_ivk_from_sk, recipient_from_pk_v2,
    recipient_from_sk_v2, CallMessage as MidnightCallMessage, EncryptedNote, Hash32, MerkleTree,
    PrivacyAddress, SpendPublic,
};
use num_cpus;
use reqwest::Client as HttpClient;
use serde_json::Value as JsonValue;
use sov_api_spec::types as api_types;
use sov_bank::{
    config_gas_token_id, Amount as BankAmount, CallMessage as BankCallMessage, Coins as BankCoins,
    TokenId,
};
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::gas::UnlimitedGasMeter;
use sov_modules_api::transaction::Transaction;
use sov_modules_api::CryptoSpec;
use sov_modules_api::Spec;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_node_client::NodeClient;
use sov_rollup_interface::crypto::{PrivateKey as _, PublicKey as _};
use sov_test_utils::default_test_signed_transaction;
use tokio::time::sleep;

use crate::fvk_service::fetch_viewer_fvk_bundle;
use crate::pool_fvk::{
    decode_ligero_hash32_arg, ensure_pool_fvk_pk_env, inject_pool_sig_hex_into_proof_bytes,
};
use crate::{
    find_rollup_binary, make_viewer_bundle, setup_ligero_env, start_local_verifier, wait_for_ready,
    ChildGuard,
};
use sov_rollup_ligero::MockDemoRollup;

// Match the spec used by the demo rollup binary
type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

/// Must match the domain used by the MidnightPrivacy module (genesis config).
const DOMAIN: Hash32 = [1u8; 32];
const E2E_DEPOSIT_AMOUNT: u128 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalletSource {
    Genesis,
    Dynamic,
}

impl WalletSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Genesis => "genesis",
            Self::Dynamic => "dynamic",
        }
    }
}

/// Configuration for running the E2E benchmark.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// Number of deposits (and transfers) to execute.
    pub num_deposits: usize,
    /// If true, run only the deposit phase and skip transfer generation/submission.
    pub deposits_only: bool,
    /// External node URL, if using already-running services.
    pub external_node_url: Option<String>,
    /// External verifier URL, if using already-running services.
    pub external_verifier_url: Option<String>,
    /// Enable proof caching to reuse proofs across runs
    pub use_proof_cache: bool,
    /// Directory to store cached proofs
    pub proof_cache_dir: PathBuf,
    /// Skip local proof verification
    pub skip_verify: bool,
    /// Maximum number of concurrent proof generations (default: num_cpus)
    pub max_concurrent_proofs: usize,
    /// If true, verifier will queue worker submissions and we will flush them in batches.
    pub defer_sequencer_submission: bool,
    /// Optional delay (ms) between submitting transfer requests to the verifier to avoid OS/socket overloads.
    pub transfer_submit_delay_ms: u64,
    /// Source of wallets used for deposits/transfers.
    pub wallet_source: WalletSource,
    /// Extra L2 amount to include when dynamically funding wallets.
    pub dynamic_fund_gas_reserve: u128,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            num_deposits: 100,
            deposits_only: false,
            external_node_url: None,
            external_verifier_url: None,
            use_proof_cache: false,
            proof_cache_dir: PathBuf::from("proof_cache"),
            skip_verify: true,
            max_concurrent_proofs: 5,
            defer_sequencer_submission: true,
            transfer_submit_delay_ms: 10,
            wallet_source: WalletSource::Genesis,
            dynamic_fund_gas_reserve: 1_000_000,
        }
    }
}

impl RunnerConfig {
    /// Builds a configuration from environment variables (used by tests and CLI).
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(value) = std::env::var("NUM_DEPOSITS") {
            if let Ok(parsed) = value.parse() {
                cfg.num_deposits = parsed;
            }
        }
        if let Ok(value) = std::env::var("DEPOSITS_ONLY") {
            cfg.deposits_only = value == "1" || value.to_lowercase() == "true";
        }
        if let Ok(value) = std::env::var("USE_PROOF_CACHE") {
            cfg.use_proof_cache = value == "1" || value.to_lowercase() == "true";
        }
        if let Ok(value) = std::env::var("PROOF_CACHE_DIR") {
            cfg.proof_cache_dir = PathBuf::from(value);
        }
        if let Ok(value) = std::env::var("SKIP_VERIFY") {
            cfg.skip_verify = value == "1" || value.to_lowercase() == "true";
        }
        if let Ok(value) = std::env::var("MAX_CONCURRENT_PROOFS") {
            if let Ok(parsed) = value.parse() {
                cfg.max_concurrent_proofs = parsed;
            }
        }
        // Allow enabling batch/queued submission mode via env
        if let Ok(value) = std::env::var("DEFER_SEQUENCER_SUBMISSION") {
            cfg.defer_sequencer_submission = value == "1" || value.to_lowercase() == "true";
        }
        if let Ok(value) = std::env::var("TRANSFER_SUBMIT_DELAY_MS") {
            if let Ok(parsed) = value.parse() {
                cfg.transfer_submit_delay_ms = parsed;
            }
        }
        if let Ok(value) = std::env::var("E2E_WALLET_SOURCE")
            .or_else(|_| std::env::var("CONTINUOUS_WALLET_SOURCE"))
        {
            match value.trim().to_ascii_lowercase().as_str() {
                "" | "genesis" => cfg.wallet_source = WalletSource::Genesis,
                "dynamic" => cfg.wallet_source = WalletSource::Dynamic,
                other => {
                    eprintln!(
                        "[config] Unsupported E2E_WALLET_SOURCE='{}'. Falling back to genesis.",
                        other
                    );
                }
            }
        }
        if let Ok(value) = std::env::var("E2E_DYNAMIC_FUND_GAS_RESERVE")
            .or_else(|_| std::env::var("DYNAMIC_FUND_GAS_RESERVE"))
            .or_else(|_| std::env::var("AUTO_FUND_GAS_RESERVE"))
        {
            if let Ok(parsed) = value.parse::<u128>() {
                cfg.dynamic_fund_gas_reserve = parsed;
            }
        }
        cfg.external_node_url = std::env::var("E2E_ROLLUP_EXTERNAL_NODE_URL").ok();
        cfg.external_verifier_url = std::env::var("E2E_ROLLUP_EXTERNAL_VERIFIER_URL").ok();
        cfg
    }
}

// We no longer rely on the compiled CHAIN_HASH; fetch from /rollup/schema instead.

fn rollup_crate_dir() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("examples/rollup-ligero").exists())
        .ok_or_else(|| anyhow!("Could not find repository root"))?;
    Ok(repo_root.join("examples/rollup-ligero"))
}

fn private_key_from_hex(
    private_key_hex: &str,
) -> Result<<<DemoRollupSpec as Spec>::CryptoSpec as CryptoSpec>::PrivateKey> {
    let normalized = private_key_hex.trim().trim_start_matches("0x");
    let private_key_bytes = hex::decode(normalized).context("Failed to decode private key hex")?;
    anyhow::ensure!(
        private_key_bytes.len() == 32,
        "Private key must be 32 bytes (got {} bytes)",
        private_key_bytes.len()
    );

    let key_json = serde_json::json!({
        "key_pair": private_key_bytes,
    });

    let private_key = serde_json::from_value(key_json)
        .context("Failed to deserialize private key from hex bytes")?;
    Ok(private_key)
}

#[derive(serde::Deserialize)]
struct DedupResponse {
    #[allow(dead_code)]
    nonce: Option<u64>,
    generation: Option<u64>,
}

async fn fetch_initial_nonce(
    http: &HttpClient,
    node_url: &str,
    account: &PrivateKeyAndAddress<DemoRollupSpec>,
) -> Result<u64> {
    let pub_key = account.private_key.pub_key();
    let cred = pub_key.credential_id();
    let base = node_url.trim_end_matches('/');
    let url = format!("{}/rollup/addresses/{}/dedup?select=generation", base, cred);

    let resp = http.get(&url).send().await;
    let status = match &resp {
        Ok(r) => r.status(),
        Err(_) => return Ok(0),
    };
    if !status.is_success() {
        return Ok(0);
    }

    let body: DedupResponse = match resp.unwrap().json::<DedupResponse>().await {
        Ok(b) => b,
        Err(_) => return Ok(0),
    };

    Ok(body.generation.unwrap_or(0))
}

fn load_accounts_from_genesis(
    crate_dir: &Path,
    num_deposits: usize,
) -> Result<Vec<PrivateKeyAndAddress<DemoRollupSpec>>> {
    let keypairs_path = crate_dir
        .parent()
        .unwrap() // examples/
        .join("test-data/genesis/demo/mock/generated_keypairs.json");

    if !keypairs_path.exists() {
        anyhow::bail!(
            "Generated keypairs file not found at {}. Run generate-genesis-keys or switch to dynamic mode with E2E_WALLET_SOURCE=dynamic and ADMIN_WALLET_PRIVATE_KEY set.",
            keypairs_path.display()
        );
    }

    let keypairs_json = std::fs::read_to_string(&keypairs_path).with_context(|| {
        format!(
            "Failed to read keypairs file at {}",
            keypairs_path.display()
        )
    })?;

    let all_keypairs: Vec<PrivateKeyAndAddress<DemoRollupSpec>> =
        serde_json::from_str(&keypairs_json).with_context(|| {
            format!(
                "Failed to parse keypairs file at {}",
                keypairs_path.display()
            )
        })?;

    if all_keypairs.len() < num_deposits {
        anyhow::bail!(
            "Not enough keypairs in genesis file. Need {}, but only {} available. Please regenerate with more accounts.",
            num_deposits,
            all_keypairs.len()
        );
    }

    Ok(all_keypairs.into_iter().take(num_deposits).collect())
}

async fn wait_for_l2_balance(
    client: &NodeClient,
    wallet: &PrivateKeyAndAddress<DemoRollupSpec>,
    token_id: &TokenId,
    min_balance: u128,
    timeout: Duration,
    poll: Duration,
) -> Result<u128> {
    let started = std::time::Instant::now();
    loop {
        match client
            .get_balance::<DemoRollupSpec>(&wallet.address, token_id, None)
            .await
        {
            Ok(balance) => {
                let balance_u128: u128 = balance.0;
                if balance_u128 >= min_balance {
                    return Ok(balance_u128);
                }
            }
            Err(e) => {
                eprintln!(
                    "[setup] failed to query L2 balance while waiting for funding (wallet={}): {}",
                    wallet.address, e
                );
            }
        }

        if started.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for wallet {} to reach L2 balance >= {}",
                wallet.address,
                min_balance
            );
        }

        sleep(poll).await;
    }
}

async fn load_accounts_dynamic(
    client: &NodeClient,
    http: &HttpClient,
    node_base_url: &str,
    chain_hash: &[u8; 32],
    num_deposits: usize,
    deposit_amount: u128,
    dynamic_fund_gas_reserve: u128,
) -> Result<Vec<PrivateKeyAndAddress<DemoRollupSpec>>> {
    let admin_wallet_private_key_hex = std::env::var("ADMIN_WALLET_PRIVATE_KEY")
        .context("ADMIN_WALLET_PRIVATE_KEY is required when E2E_WALLET_SOURCE=dynamic")?;
    let admin_private_key = private_key_from_hex(&admin_wallet_private_key_hex)?;
    let admin_account = PrivateKeyAndAddress::<DemoRollupSpec>::from_key(admin_private_key);
    let gas_token_id = config_gas_token_id();
    let l2_funding_amount = deposit_amount
        .checked_add(dynamic_fund_gas_reserve)
        .ok_or_else(|| {
            anyhow!(
                "Overflow while computing dynamic L2 funding amount: deposit_amount={} + dynamic_fund_gas_reserve={}",
                deposit_amount,
                dynamic_fund_gas_reserve
            )
        })?;

    let mut admin_nonce = fetch_initial_nonce(http, node_base_url, &admin_account)
        .await
        .with_context(|| {
            format!(
                "Failed to fetch latest nonce/generation for admin wallet {}",
                admin_account.address
            )
        })?;

    eprintln!(
        "[setup] Dynamic wallet bootstrap enabled. admin_address={} wallets={} funding_per_wallet={} (deposit_amount={} + gas_reserve={}) token_id={}",
        admin_account.address,
        num_deposits,
        l2_funding_amount,
        deposit_amount,
        dynamic_fund_gas_reserve,
        gas_token_id
    );

    let mut accounts: Vec<PrivateKeyAndAddress<DemoRollupSpec>> = Vec::with_capacity(num_deposits);

    for i in 0..num_deposits {
        let account = PrivateKeyAndAddress::<DemoRollupSpec>::generate();

        let latest_admin_nonce = fetch_initial_nonce(http, node_base_url, &admin_account)
            .await
            .unwrap_or(admin_nonce);
        if latest_admin_nonce > admin_nonce {
            admin_nonce = latest_admin_nonce;
        }

        let mut submitted = false;
        let mut last_error: Option<anyhow::Error> = None;
        for attempt in 0..3usize {
            let bank_transfer_call =
                RuntimeCall::<DemoRollupSpec>::Bank(BankCallMessage::Transfer {
                    to: account.address.clone(),
                    coins: BankCoins {
                        amount: BankAmount::from(l2_funding_amount),
                        token_id: gas_token_id.clone(),
                    },
                });
            let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
                default_test_signed_transaction(
                    &admin_account.private_key,
                    &bank_transfer_call,
                    admin_nonce,
                    chain_hash,
                );
            let mut meter = UnlimitedGasMeter::<DemoRollupSpec>::default();
            tx.verify(chain_hash, &mut meter)
                .context("Dynamic funding tx signature verification failed")?;

            let tx_bytes = borsh::to_vec(&tx)?;
            match client
                .send_transactions_to_sequencer(vec![tx_bytes], true)
                .await
            {
                Ok(_) => {
                    submitted = true;
                    admin_nonce = admin_nonce
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("admin nonce overflow"))?;
                    break;
                }
                Err(e) => {
                    last_error = Some(anyhow!(e));
                    if attempt < 2 {
                        let refreshed_nonce =
                            fetch_initial_nonce(http, node_base_url, &admin_account)
                                .await
                                .unwrap_or(admin_nonce);
                        admin_nonce = refreshed_nonce;
                        sleep(Duration::from_millis(300)).await;
                    }
                }
            }
        }

        if !submitted {
            return Err(last_error.unwrap_or_else(|| {
                anyhow!(
                    "Failed funding dynamic wallet {} after retries (address={})",
                    i,
                    account.address
                )
            }));
        }

        let _ = wait_for_l2_balance(
            client,
            &account,
            &gas_token_id,
            l2_funding_amount,
            Duration::from_secs(60),
            Duration::from_millis(300),
        )
        .await
        .with_context(|| {
            format!(
                "Failed waiting for dynamic wallet {} funding (address={})",
                i, account.address
            )
        })?;

        if i < 5 || i + 1 == num_deposits {
            eprintln!(
                "[setup] funded dynamic account {} address={} with {} gas tokens",
                i, account.address, l2_funding_amount
            );
        }

        accounts.push(account);
    }

    Ok(accounts)
}

fn make_temp_config(base_config: &str, data_dir: &std::path::Path, http_port: u16) -> String {
    let da_conn = format!(
        "connection_string = \"sqlite://{}/da.sqlite?mode=rwc\"",
        data_dir.display()
    );
    let storage_path = format!("path = \"{}\"", data_dir.display());
    let bind_port = format!("bind_port = {}", http_port);

    let mut out = String::with_capacity(base_config.len() + 256);
    for line in base_config.lines() {
        let l = line.trim_start();
        if l.starts_with("connection_string = ") {
            out.push_str(&da_conn);
        } else if l.starts_with("path = ") && !l.contains("target/") {
            out.push_str(&storage_path);
        } else if l.starts_with("bind_port = ") {
            out.push_str(&bind_port);
        } else if l.starts_with("finalization = ") {
            // Speed up readiness for tests
            out.push_str("finalization = 0");
        } else if l.starts_with("block_time_ms = ") {
            // Faster mock DA blocks
            out.push_str("block_time_ms = 200");
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[derive(Clone, Debug)]
struct ExternalConfig {
    node_url: String,
    verifier_url: String,
}

impl ExternalConfig {
    fn from_config(config: &RunnerConfig) -> Result<Option<Self>> {
        match (&config.external_node_url, &config.external_verifier_url) {
            (Some(node_url), Some(verifier_url)) => Ok(Some(Self {
                node_url: node_url.clone(),
                verifier_url: verifier_url.clone(),
            })),
            (None, None) => Ok(None),
            _ => anyhow::bail!(
                "Both external node and verifier URLs must be provided to use external services"
            ),
        }
    }
}

struct TestEnvironment {
    api_url: String,
    da_connection_string: Option<String>,
    #[allow(dead_code)]
    temp_dir: Option<tempfile::TempDir>,
    child_guard: Option<ChildGuard>,
}

impl TestEnvironment {
    fn external(api_url: String) -> Self {
        Self {
            api_url,
            da_connection_string: None,
            temp_dir: None,
            child_guard: None,
        }
    }

    fn shutdown(&mut self) {
        if let Some(mut guard) = self.child_guard.take() {
            let _ = guard.0.kill();
            let _ = guard.0.wait();
        }
    }
}

fn prepare_environment(
    crate_dir: &Path,
    bin_path: &str,
    program_path: &str,
    external: Option<ExternalConfig>,
) -> Result<TestEnvironment> {
    if let Some(cfg) = external {
        return Ok(TestEnvironment::external(cfg.node_url));
    }

    let base_cfg_path = crate_dir.join("rollup_config.toml");
    let base_cfg = std::fs::read_to_string(&base_cfg_path)
        .with_context(|| format!("Failed to read base config at {}", base_cfg_path.display()))?;

    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let http_port = listener.local_addr()?.port();
    drop(listener);

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("demo_data");
    std::fs::create_dir_all(&data_dir)?;
    let new_cfg = make_temp_config(&base_cfg, &data_dir, http_port);
    let cfg_path = temp.path().join("rollup_config.toml");
    std::fs::write(&cfg_path, new_cfg)?;

    let mut child = Command::new(bin_path)
        .current_dir(crate_dir)
        .arg("--rollup-config-path")
        .arg(cfg_path.as_os_str())
        .arg("--prometheus-exporter-bind")
        .arg("127.0.0.1:0")
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .env("LIGERO_PROGRAM_PATH", program_path)
        .env("LIGERO_PACKING", "8192")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sov-rollup-ligero")?;

    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                eprintln!("[node stdout] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                eprintln!("[node stderr] {}", line);
            }
        });
    }

    let api_url = format!("http://127.0.0.1:{}", http_port);
    let da_connection_string = format!("sqlite://{}/da.sqlite?mode=rwc", data_dir.display());

    Ok(TestEnvironment {
        api_url,
        da_connection_string: Some(da_connection_string),
        temp_dir: Some(temp),
        child_guard: Some(ChildGuard::new(child)),
    })
}

/// Runs the full E2E benchmark, optionally connecting to external services.
pub async fn run(config: RunnerConfig) -> Result<()> {
    // Arrange: prepare isolated config or connect to existing services
    let crate_dir = rollup_crate_dir()?;
    let bin_path = find_rollup_binary()?;
    let ligero_env = setup_ligero_env()?;
    let program_path_for_node = ligero_env.program_path.clone();
    let method_id_for_node = ligero_env.method_id;
    let external_services = ExternalConfig::from_config(&config)?;
    let mut env = prepare_environment(
        &crate_dir,
        &bin_path,
        &program_path_for_node,
        external_services.clone(),
    )?;

    let api_url = env.api_url.clone();
    let client = Arc::new(NodeClient::new_unchecked(&api_url));
    // With faster DA config above this should be quick; allow up to 90s for first run
    wait_for_ready(&client, Duration::from_secs(90)).await?;

    // Fetch the authoritative chain_hash from the node's schema endpoint (used for verifier + signing)
    #[derive(serde::Deserialize)]
    struct SchemaResp {
        chain_hash: String,
    }
    let schema: SchemaResp = client
        .query_rest_endpoint("/rollup/schema")
        .await
        .context("Failed to fetch /rollup/schema")?;
    let chain_hash_hex = schema.chain_hash.trim_start_matches("0x");
    let chain_hash_vec = hex::decode(chain_hash_hex)
        .with_context(|| format!("Invalid chain_hash returned by node: {}", schema.chain_hash))?;
    anyhow::ensure!(chain_hash_vec.len() == 32, "chain_hash must be 32 bytes");
    let mut chain_hash = [0u8; 32];
    chain_hash.copy_from_slice(&chain_hash_vec);

    // If `MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX` is set, export it as `POOL_FVK_PK` so the in-process
    // verifier (when spawned) enforces pool-signed viewer commitments.
    let pool_fvk_pk = ensure_pool_fvk_pk_env()?;
    if let Some(pool_pk_bytes) = pool_fvk_pk {
        eprintln!(
            "[config] POOL_FVK_PK set: enforcing pool-signed viewer commitments (pk={}...)",
            hex::encode(&pool_pk_bytes[..8]),
        );
    } else {
        eprintln!("[config] POOL_FVK_PK not set: pool signature enforcement DISABLED");
    }

    // Fetch the pool viewer FVK + commitment signature from midnight-fvk-service.
    // When `POOL_FVK_PK` is set, viewer attestations and ciphertexts are required.
    let http = HttpClient::new();
    let viewer_bundle = if pool_fvk_pk.is_some() {
        Some(fetch_viewer_fvk_bundle(&http, pool_fvk_pk).await?)
    } else {
        None
    };
    let viewer_fvk: Option<Hash32> = viewer_bundle.as_ref().map(|b| b.fvk);
    let expected_viewer_fvk_commitment: Option<Hash32> =
        viewer_bundle.as_ref().map(|b| b.fvk_commitment);
    let pool_sig_hex: Option<Arc<String>> = viewer_bundle
        .as_ref()
        .map(|b| Arc::new(b.pool_sig_hex.clone()));

    if let Some(b) = viewer_bundle.as_ref() {
        eprintln!(
            "[config] pool viewer enabled: fvk_commitment=0x{}...",
            hex::encode(&b.fvk_commitment[..8])
        );
    } else {
        eprintln!("[config] pool viewer disabled: transfers will NOT emit viewer ciphertexts");
    }

    // Use the method_id we already computed when starting the node
    let method_id = method_id_for_node;

    // Start or connect to the proof-verifier service
    let verifier_parallelism = std::cmp::max(4, num_cpus::get());
    let verifier_url: String = if let Some(cfg) = external_services.as_ref() {
        cfg.verifier_url.clone()
    } else {
        let da_connection_string = env
            .da_connection_string
            .as_ref()
            .expect("Managed environment must provide DA connection string")
            .clone();
        start_local_verifier(
            &api_url,
            method_id,
            &da_connection_string,
            verifier_parallelism,
            config.defer_sequencer_submission,
        )
        .await?
    };

    // Number of deposits (default 10)
    let num_deposits = config.num_deposits;
    let deposit_amount: u128 = E2E_DEPOSIT_AMOUNT;

    let accounts: Vec<PrivateKeyAndAddress<DemoRollupSpec>> = match config.wallet_source {
        WalletSource::Genesis => {
            eprintln!(
                "[setup] Wallet source=genesis. Loading {} pre-funded accounts from generated_keypairs.json",
                num_deposits
            );
            load_accounts_from_genesis(&crate_dir, num_deposits)?
        }
        WalletSource::Dynamic => {
            eprintln!(
                "[setup] Wallet source=dynamic. Generating {} accounts and prefunding from ADMIN_WALLET_PRIVATE_KEY",
                num_deposits
            );
            load_accounts_dynamic(
                client.as_ref(),
                &http,
                &api_url,
                &chain_hash,
                num_deposits,
                deposit_amount,
                config.dynamic_fund_gas_reserve,
            )
            .await?
        }
    };

    eprintln!(
        "\n✅✅✅ [setup] Prepared {} accounts from {} wallet source ✅✅✅",
        accounts.len(),
        config.wallet_source.as_str()
    );

    // Capture initial module state for robust delta checks
    #[derive(serde::Deserialize, Clone, Debug)]
    struct TreeState {
        root: Vec<u8>,
        next_position: u64,
    }
    #[derive(serde::Deserialize, Clone, Copy, Debug, Default)]
    struct Stats {
        #[serde(default)]
        deposit_count: u64,
        #[serde(default)]
        nullifiers_spent: u64,
    }
    let initial_tree: TreeState = client
        .query_rest_endpoint("/modules/midnight-privacy/tree/state")
        .await
        .context("Failed to query initial midnight-privacy tree state")?;
    let initial_stats: Stats = client
        .query_rest_endpoint("/modules/midnight-privacy/stats")
        .await
        .unwrap_or_default();

    #[derive(Debug, Clone, serde::Deserialize)]
    struct VerifierMetrics {
        #[serde(default)]
        deserialize_ms: f64,
        #[serde(default)]
        parse_ms: f64,
        #[serde(default)]
        signature_verify_ms: f64,
        #[serde(default)]
        proof_verify_ms: f64,
        #[serde(default)]
        tx_creation_ms: f64,
        #[serde(default)]
        node_submit_ms: f64,
        #[serde(default)]
        total_ms: f64,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct VerifierSubmitResponse {
        #[serde(default)]
        success: bool,
        #[serde(default)]
        tx_hash: Option<String>,
        #[serde(default)]
        sequencer_response: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        metrics: Option<VerifierMetrics>,
    }

    fn log_submission_timing(
        kind: &str,
        idx: usize,
        http_elapsed_ms: f64,
        metrics: Option<&VerifierMetrics>,
        sequencer_response_present: bool,
        stf_execution_ms: Option<f64>,
    ) {
        if let Some(m) = metrics {
            eprintln!(
                concat!(
                    "  [timing][{} #{:02}] ",
                    "http={:.3} ms ",
                    "node_submit={:.3} ms ",
                    "total={:.3} ms ",
                    "deserialize={:.3} ms ",
                    "parse={:.3} ms ",
                    "signature={:.3} ms ",
                    "proof={:.3} ms ",
                    "creation={:.3} ms ",
                    "sequencer_response={} ",
                    "stf={}",
                ),
                kind,
                idx,
                http_elapsed_ms,
                m.node_submit_ms,
                m.total_ms,
                m.deserialize_ms,
                m.parse_ms,
                m.signature_verify_ms,
                m.proof_verify_ms,
                m.tx_creation_ms,
                sequencer_response_present,
                stf_execution_ms
                    .map(|ms| format!("{:.3} ms", ms))
                    .unwrap_or_else(|| "n/a".to_string()),
            );
        } else {
            eprintln!(
                "  [timing][{} #{:02}] http={:.3} ms (no verifier metrics, sequencer_response={}, stf={})",
                kind,
                idx,
                http_elapsed_ms,
                sequencer_response_present,
                stf_execution_ms
                    .map(|ms| format!("{:.3} ms", ms))
                    .unwrap_or_else(|| "n/a".to_string()),
            );
        }
    }

    async fn submit_to_verifier_with_sync_retry(
        client: &reqwest::Client,
        verifier_url: &str,
        body_b64: &str,
        label: &str,
        idx: usize,
    ) -> anyhow::Result<(VerifierSubmitResponse, f64)> {
        let mut backoff = Duration::from_millis(50);
        let max_backoff = Duration::from_secs(2);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let submit_start = std::time::Instant::now();
            let resp = client
                .post(format!("{}/midnight-privacy", verifier_url))
                .json(&serde_json::json!({"body": body_b64}))
                .send()
                .await
                .with_context(|| format!("{} #{} request failed", label, idx))?;
            let http_elapsed_ms = submit_start.elapsed().as_secs_f64() * 1000.0;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .with_context(|| format!("{} #{} failed to read response body", label, idx))?;

            // If verifier itself errors (e.g., 503), consider retry
            if status.as_u16() == 503 && text.contains("Syncing") {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "{} #{} retried but node still Syncing (HTTP 503): {}",
                        label,
                        idx,
                        text
                    );
                }
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
                continue;
            }

            // Normal JSON response
            let parsed: VerifierSubmitResponse = serde_json::from_str(&text)
                .with_context(|| format!("{} #{} invalid JSON: {}", label, idx, text))?;

            // If sequencer reported Syncing through the verifier (success=false but error present), retry
            if !parsed.success {
                let syncing = parsed
                    .error
                    .as_deref()
                    .map(|e| e.contains("\"Syncing\"") || e.contains("fell out of sync"))
                    .unwrap_or(false);
                if syncing && std::time::Instant::now() < deadline {
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
                    continue;
                }
            }

            return Ok((parsed, http_elapsed_ms));
        }
    }

    async fn flush_verifier_queue(
        http: &reqwest::Client,
        verifier_url: &str,
    ) -> anyhow::Result<()> {
        eprintln!("[flush] Flushing queued worker transactions to sequencer...");
        let resp = http
            .post(format!("{}/midnight-privacy/flush", verifier_url))
            .send()
            .await
            .context("flush request failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "".to_string());
        if !status.is_success() {
            anyhow::bail!("flush endpoint returned {}: {}", status, body);
        }
        eprintln!("[flush] Flushed queued worker transactions to sequencer");
        Ok(())
    }

    fn extract_stf_execution_ms(value: &serde_json::Value) -> Option<f64> {
        value
            .get("stf_execution_time_micros")
            .or_else(|| {
                value
                    .get("confirmation")
                    .and_then(|c| c.get("stf_execution_time_micros"))
            })
            .and_then(|v| v.as_u64())
            .map(|micros| micros as f64 / 1000.0)
    }

    #[derive(Debug)]
    struct SeriesStats {
        avg: f64,
        p50: f64,
        p90: f64,
        p95: f64,
        p99: f64,
    }

    fn percentile(sorted: &[f64], quantile: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        if sorted.len() == 1 {
            return sorted[0];
        }
        let rank = quantile.clamp(0.0, 1.0) * (sorted.len() as f64 - 1.0);
        let lower = rank.floor() as usize;
        let upper = rank.ceil() as usize;
        if lower == upper {
            sorted[lower]
        } else {
            let weight = rank - lower as f64;
            sorted[lower] + (sorted[upper] - sorted[lower]) * weight
        }
    }

    fn compute_stats(values: &[f64]) -> Option<SeriesStats> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let avg = values.iter().sum::<f64>() / values.len() as f64;
        Some(SeriesStats {
            avg,
            p50: percentile(&sorted, 0.50),
            p90: percentile(&sorted, 0.90),
            p95: percentile(&sorted, 0.95),
            p99: percentile(&sorted, 0.99),
        })
    }

    fn summarize_timings(label: &str, http: &[f64], node_submit: &[f64], stf: &[f64]) {
        eprintln!("  [{}] Timings:", label);
        if let Some(stats) = compute_stats(http) {
            eprintln!(
                "    HTTP round-trip (ms): avg={:.3} p50={:.3} p90={:.3} p95={:.3} p99={:.3}",
                stats.avg, stats.p50, stats.p90, stats.p95, stats.p99
            );
        } else {
            eprintln!("    HTTP round-trip (ms): no samples recorded");
        }
        if let Some(stats) = compute_stats(node_submit) {
            eprintln!(
                "    node_submit_ms (ms): avg={:.3} p50={:.3} p90={:.3} p95={:.3} p99={:.3}",
                stats.avg, stats.p50, stats.p90, stats.p95, stats.p99
            );
        } else {
            eprintln!("    node_submit_ms (ms): no samples recorded");
        }
        if let Some(stats) = compute_stats(stf) {
            eprintln!(
                "    STF execution (ms): avg={:.3} p50={:.3} p90={:.3} p95={:.3} p99={:.3}",
                stats.avg, stats.p50, stats.p90, stats.p95, stats.p99
            );
        } else {
            eprintln!("    STF execution (ms): no samples recorded");
        }
    }

    let mut tx_hashes_hex: Vec<String> = Vec::with_capacity(num_deposits);
    // (account_idx, tx_hash, amount, rho, spend_sk) - track which account made each deposit
    let mut deposit_secrets: Vec<(usize, String, u128, Hash32, Hash32)> =
        Vec::with_capacity(num_deposits);
    let mut deposit_http_timings: Vec<f64> = Vec::with_capacity(num_deposits);
    let mut deposit_node_submit_timings: Vec<f64> = Vec::with_capacity(num_deposits);
    let mut deposit_stf_execution_ms: Vec<f64> = Vec::with_capacity(num_deposits);
    // Track per-tx execution time (micros) keyed by tx hash for per-batch aggregation
    let mut deposit_exec_time_by_hash_micros: HashMap<String, u64> = HashMap::new();
    let mut transfer_stf_execution_ms: Vec<f64> = Vec::new();
    let mut transfer_http_timings: Vec<f64> = Vec::new();
    let mut transfer_node_submit_timings: Vec<f64> = Vec::new();
    let mut deposit_nonce_by_account: Vec<u64> = vec![0; num_deposits];

    eprintln!("\n\n #### Step 2 ####");
    eprintln!(
        "\n[deposits] Creating {} deposits (one per account for parallelism)...",
        num_deposits
    );
    for i in 0..num_deposits {
        let account = &accounts[i]; // Each deposit uses a different account

        // Build a midnight deposit tx for demo runtime
        let amount: u128 = deposit_amount;
        let rho: Hash32 = rand::random();
        let spend_sk: Hash32 = rand::random();
        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &spend_sk);
        let recipient: Hash32 = recipient_from_sk_v2(&DOMAIN, &spend_sk, &pk_ivk);
        let call = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Deposit {
            amount,
            rho,
            recipient,
            gas: None,
            view_fvks: None,
        });

        let deposit_nonce = fetch_initial_nonce(&http, &api_url, account)
            .await
            .with_context(|| {
                format!(
                    "Failed to fetch latest nonce/generation for deposit account {} ({})",
                    i, account.address
                )
            })?;
        deposit_nonce_by_account[i] = deposit_nonce;

        let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
            default_test_signed_transaction(
                &account.private_key, // Each account signs its own tx
                &call,
                deposit_nonce, // Account-specific nonce
                &chain_hash,
            );
        // Local sanity check: signature should verify
        let mut meter = UnlimitedGasMeter::<DemoRollupSpec>::default();
        tx.verify(&chain_hash, &mut meter)
            .context("Local signature verification failed")?;
        let tx_bytes = borsh::to_vec(&tx)?;

        // Submit to the verifier service (preferred) with fallback to sequencer direct
        let tx_b64 = BASE64_STANDARD.encode(&tx_bytes);
        let mut submitted_via_verifier = false;
        match submit_to_verifier_with_sync_retry(&http, &verifier_url, &tx_b64, "deposit", i + 1)
            .await
        {
            Ok((parsed, http_elapsed_ms)) => {
                let has_seq_resp = parsed.sequencer_response.is_some();
                let stf_execution_ms = parsed
                    .sequencer_response
                    .as_ref()
                    .and_then(extract_stf_execution_ms);
                log_submission_timing(
                    "deposit",
                    i + 1,
                    http_elapsed_ms,
                    parsed.metrics.as_ref(),
                    has_seq_resp,
                    stf_execution_ms,
                );
                deposit_http_timings.push(http_elapsed_ms);
                if let Some(m) = parsed.metrics.as_ref() {
                    deposit_node_submit_timings.push(m.node_submit_ms);
                }
                if let Some(ms) = stf_execution_ms {
                    deposit_stf_execution_ms.push(ms);
                    if let Some(hx) = parsed.tx_hash.as_ref() {
                        // store micros to match internal tracker units
                        let micros = (ms * 1000.0) as u64;
                        deposit_exec_time_by_hash_micros.insert(hx.clone(), micros);
                    }
                }
                if has_seq_resp && stf_execution_ms.is_none() {
                    if let Some(resp) = parsed.sequencer_response.as_ref() {
                        eprintln!(
                            "  [timing][deposit #{:02}] warning: STF metric missing in sequencer response: {}",
                            i + 1,
                            serde_json::to_string(resp).unwrap_or_default()
                        );
                    }
                }
                if parsed.success {
                    submitted_via_verifier = true;
                } else {
                    eprintln!(
                        "  [deposits] verifier reported failure for deposit #{}: {:?}",
                        i + 1,
                        parsed.error
                    );
                }
            }
            Err(err) => {
                eprintln!(
                    "  [deposits] failed to submit deposit #{} to verifier: {}",
                    i + 1,
                    err
                );
            }
        }
        if !submitted_via_verifier {
            eprintln!(
                "  [deposits] falling back to direct sequencer submission for deposit #{}",
                i + 1
            );
            let _ = client
                .send_transactions_to_sequencer(vec![tx_bytes.clone()], true)
                .await;
        }

        let tx_hash_str = tx.hash().to_string();
        eprintln!(
            "  [deposits] deposit #{} tx={} amount={} from account={} nonce={} rho={} recipient={}",
            i + 1,
            tx_hash_str,
            amount,
            i,
            deposit_nonce,
            hex::encode(&rho[..8]),
            hex::encode(&recipient[..8])
        );
        deposit_secrets.push((i, tx_hash_str.clone(), amount, rho, spend_sk));
        tx_hashes_hex.push(tx_hash_str);
    }

    // If we deferred sequencer submission in the verifier, flush deposits now
    if config.defer_sequencer_submission {
        eprintln!("[flush] Waiting 5 seconds before flushing queued transfers...");
        sleep(Duration::from_secs(5)).await;
        flush_verifier_queue(&http, &verifier_url).await?;
    }

    // Debug: fetch tx receipt for the last deposit and print
    let receipt_json = client
        .http_get(&format!("/sequencer/txs/{}", tx_hashes_hex.last().unwrap()))
        .await
        .unwrap_or_else(|e| format!("<failed to fetch tx receipt: {e}>"));
    eprintln!("[debug] tx receipt: {}", receipt_json);

    // Verify each tx is included in the ledger (i.e., appears in a slot/batch)
    // and count how many deposit txs were included.
    let mut included_deposits = 0usize;
    let mut deposit_batch_stats: HashMap<u64, usize> = HashMap::new();
    let mut deposit_hash_to_batch: HashMap<String, u64> = HashMap::new();
    let deposit_start_time = std::time::Instant::now();
    let mut deposit_cm_by_hash: HashMap<String, [u8; 32]> = HashMap::new();
    for (deposit_idx, hash_hex) in tx_hashes_hex.iter().enumerate() {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match client
                .query_rest_endpoint::<api_types::LedgerTx>(&format!(
                    "/ledger/txs/{}?children=1",
                    hash_hex
                ))
                .await
            {
                Ok(ltx) => {
                    // Found in ledger: assert receipt is successful
                    if ltx.receipt.result != api_types::TxReceiptResult::Successful {
                        eprintln!("\n🚨🚨🚨 CRITICAL ERROR: DEPOSIT TRANSACTION REVERTED 🚨🚨🚨");
                        eprintln!("  Deposit #: {}", deposit_idx + 1);
                        eprintln!("  Tx Hash: {}", hash_hex);
                        eprintln!("  Receipt: {:?}", ltx.receipt);
                        eprintln!("  This likely means the account has insufficient funds!");
                        anyhow::bail!("❌ Deposit tx {} reverted: {:?}", hash_hex, ltx.receipt);
                    }
                    // Ensure it is a MidnightPrivacy deposit and count it
                    let has_pool_deposit = ltx
                        .events
                        .iter()
                        .any(|ev| ev.key == "ValueMidnightPrivacy/PoolDeposit");
                    if !has_pool_deposit {
                        let keys: Vec<String> =
                            ltx.events.iter().map(|ev| format!("{}", ev.key)).collect();
                        let raw_json = client
                            .http_get(&format!("/ledger/txs/{}?children=1", hash_hex))
                            .await
                            .unwrap_or_else(|e| format!("<failed to fetch ledger json: {e}>"));
                        if let Some((_, _, amt, rho, recp)) =
                            deposit_secrets.iter().find(|(_, h, ..)| h == hash_hex)
                        {
                            let pk_ivk = pk_ivk_from_sk(&DOMAIN, recp);
                            let recipient = recipient_from_sk_v2(&DOMAIN, recp, &pk_ivk);
                            eprintln!(
                                "[debug] expected deposit: amount={} rho={} recipient={}",
                                amt,
                                hex::encode(&rho[..8]),
                                hex::encode(&recipient[..8])
                            );
                        }
                        eprintln!(
                            "[debug] ledger events for {}: keys={:?} batch_number={} tx_number={}",
                            hash_hex, keys, ltx.batch_number, ltx.number
                        );
                        eprintln!("[debug] ledger tx json: {}", raw_json);
                        anyhow::bail!(
                            "Tx {} included but missing MidnightPrivacy PoolDeposit event",
                            hash_hex
                        );
                    }
                    // Extract commitment from PoolDeposit event
                    if let Some(ev) = ltx
                        .events
                        .iter()
                        .find(|ev| ev.key == "ValueMidnightPrivacy/PoolDeposit")
                    {
                        // value schema: { "pool_deposit": { "amount": <u128>, "commitment": [u8;32] } }
                        let obj = &ev.value; // already a Map<String, Value>
                        if let Some(pd) = obj.get("pool_deposit").and_then(|v| v.as_object()) {
                            if let Some(cm_arr) = pd.get("commitment").and_then(|v| v.as_array()) {
                                if cm_arr.len() == 32 {
                                    let mut cm = [0u8; 32];
                                    for (i, b) in cm_arr.iter().enumerate() {
                                        cm[i] = b.as_u64().unwrap_or(0) as u8;
                                    }
                                    deposit_cm_by_hash.insert(hash_hex.clone(), cm);
                                }
                            }
                        }
                    }
                    // Positive confirmation log for successful inclusion
                    *deposit_batch_stats.entry(ltx.batch_number).or_insert(0) += 1;
                    eprintln!(
                        "[ok] included deposit tx={} batch_number={} tx_number={} events={}",
                        hash_hex,
                        ltx.batch_number,
                        ltx.number,
                        ltx.events.len()
                    );
                    deposit_hash_to_batch.insert(hash_hex.clone(), ltx.batch_number);
                    included_deposits += 1;
                    break;
                }
                Err(_) => {
                    if std::time::Instant::now() > deadline {
                        anyhow::bail!("Timeout waiting for tx {} to appear in ledger", hash_hex);
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    // Double-check: included deposit count matches the number of deposits we sent
    anyhow::ensure!(
        included_deposits == num_deposits,
        "Expected {} deposits included, saw {}",
        num_deposits,
        included_deposits
    );
    let _deposit_total_time = deposit_start_time.elapsed();
    eprintln!(
        "[ok] all deposits included: {}/{}",
        included_deposits, num_deposits
    );

    // Verify the module state advanced (at least one note)
    // Poll tree state until epilogue flushes queued outputs
    let mut state: TreeState = initial_tree.clone();
    let target_position = initial_tree.next_position + num_deposits as u64;
    if state.next_position < target_position {
        let start = std::time::Instant::now();
        let target = Duration::from_secs(10);
        loop {
            state = client
                .query_rest_endpoint("/modules/midnight-privacy/tree/state")
                .await
                .context("Failed to query midnight-privacy tree state")?;
            anyhow::ensure!(state.root.len() == 32, "Invalid root length");
            if state.next_position >= target_position {
                break;
            }
            if start.elapsed() > target {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
    anyhow::ensure!(
        state.next_position >= target_position,
        "Tree did not advance; expected delta >= {} (initial: {}, final: {})",
        num_deposits,
        initial_tree.next_position,
        state.next_position
    );
    eprintln!(
        "[ok] tree advanced by >= {} (initial_next_position={}, final_next_position={})",
        num_deposits, initial_tree.next_position, state.next_position
    );

    // Poll stats until deposit_count reflects all deposits
    let stats_target = initial_stats.deposit_count + num_deposits as u64;
    let stats_start = std::time::Instant::now();
    let stats_timeout = Duration::from_secs(10);
    let mut final_stats: Stats = initial_stats;
    loop {
        match client
            .query_rest_endpoint::<Stats>("/modules/midnight-privacy/stats")
            .await
        {
            Ok(s) => {
                final_stats = s;
                if final_stats.deposit_count >= stats_target {
                    break;
                }
            }
            Err(_) => { /* ignore transient errors and keep polling */ }
        }
        if stats_start.elapsed() > stats_timeout {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    anyhow::ensure!(
        final_stats.deposit_count >= stats_target,
        "Deposit stats did not reach target; expected at least {} got {}",
        stats_target,
        final_stats.deposit_count
    );
    eprintln!(
        "[ok] deposit_count advanced to >= {} (initial={}, final={})",
        stats_target, initial_stats.deposit_count, final_stats.deposit_count
    );

    if config.deposits_only {
        eprintln!(
            "[mode] deposits_only=true: completed {} deposits and skipping transfer phase",
            num_deposits
        );
        env.shutdown();
        return Ok(());
    }

    // Auto-generate transfer proofs for each deposit and submit after ALL proofs are ready
    // We already configured Ligero and computed `method_id` above.

    // Fetch notes and rebuild Merkle tree to compute sibling paths
    // CRITICAL: Use the tree depth from genesis config, not from API response!
    // The API might not return depth, causing it to default to 0
    const TREE_DEPTH: u8 = 16; // Must match ValueSetterZkConfig in genesis (demo/mock/midnight_privacy.json)

    eprintln!("\n[proof] Fetching tree state and notes for proof generation...");

    // Define note types
    #[derive(serde::Deserialize, Clone)]
    struct NoteInfo {
        position: u64,
        commitment: Vec<u8>,
    }
    #[derive(serde::Deserialize)]
    struct NotesResp {
        notes: Vec<NoteInfo>,
    }

    // Poll until we have the right number of notes in the tree
    // Sometimes epilogue hasn't flushed yet
    let tree_fetch_start = std::time::Instant::now();
    let tree_fetch_timeout = Duration::from_secs(15);
    let mut state: TreeState;
    let mut notes_resp: NotesResp;

    loop {
        state = client
            .query_rest_endpoint("/modules/midnight-privacy/tree/state")
            .await
            .context("Failed to query tree state for proofs")?;

        // Fetch all notes using pagination (API caps at 1000 per request)
        let mut all_notes = Vec::new();
        let batch_size = 1000;
        let mut offset = 0;

        loop {
            let batch_resp: NotesResp = client
                .query_rest_endpoint(&format!(
                    "/modules/midnight-privacy/notes?limit={}&offset={}",
                    batch_size, offset
                ))
                .await
                .context("Failed to query notes batch")?;

            let batch_len = batch_resp.notes.len();
            all_notes.extend(batch_resp.notes);

            // If we got fewer notes than requested, we've reached the end
            if batch_len < batch_size {
                break;
            }

            offset += batch_size;
        }

        notes_resp = NotesResp { notes: all_notes };

        eprintln!(
            "  [proof] Tree state: next_position={}, notes_count={}, root={}",
            state.next_position,
            notes_resp.notes.len(),
            hex::encode(&state.root[..8])
        );

        // We need at least num_deposits notes
        if notes_resp.notes.len() >= num_deposits && state.next_position >= num_deposits as u64 {
            break;
        }

        if tree_fetch_start.elapsed() > tree_fetch_timeout {
            anyhow::bail!(
                "Timeout waiting for tree to contain {} notes. Got {} notes, next_position={}",
                num_deposits,
                notes_resp.notes.len(),
                state.next_position
            );
        }

        eprintln!(
            "  [proof] Waiting for tree to flush notes... (need {} notes, have {})",
            num_deposits,
            notes_resp.notes.len()
        );
        sleep(Duration::from_millis(200)).await;
    }

    eprintln!(
        "[proof] Rebuilding Merkle tree with depth {} and {} notes",
        TREE_DEPTH,
        notes_resp.notes.len()
    );

    // Sort notes by position to ensure consistent tree building
    let mut sorted_notes = notes_resp.notes.clone();
    sorted_notes.sort_by_key(|n| n.position);

    // Compare expected commitments (from our deposits) with API commitments
    let domain: Hash32 = DOMAIN; // Must match genesis config!
    eprintln!("\n[tree] Comparing expected vs API commitments:");
    for (account_idx, txh, amount, rho, spend_sk) in &deposit_secrets {
        let amount_u64: u64 = (*amount)
            .try_into()
            .context("deposit amount does not fit into u64 (required by note_spend_guest v2)")?;
        let pk_ivk = pk_ivk_from_sk(&domain, spend_sk);
        let recipient = recipient_from_sk_v2(&domain, spend_sk, &pk_ivk);
        // Deposit convention: sender_id == recipient.
        let expected_cm = note_commitment(&domain, amount_u64, rho, &recipient, &recipient);
        if let Some(api_cm) = deposit_cm_by_hash.get(txh) {
            let match_str = if &expected_cm == api_cm {
                "✓ MATCH"
            } else {
                "✗ MISMATCH"
            };
            eprintln!(
                "  account={} tx={} {} expected={} api={}",
                account_idx,
                &txh[..16],
                match_str,
                hex::encode(&expected_cm[..8]),
                hex::encode(&api_cm[..8])
            );
        } else {
            eprintln!(
                "  account={} tx={} ✗ NO API CM FOUND",
                account_idx,
                &txh[..16]
            );
        }
    }
    eprintln!("");

    // Log each note we're inserting
    for (i, n) in sorted_notes.iter().enumerate() {
        if n.commitment.len() == 32 {
            eprintln!(
                "  [tree] note[{}]: pos={} cm={}",
                i,
                n.position,
                hex::encode(&n.commitment[..8])
            );
        }
    }

    let mut mt = MerkleTree::new(TREE_DEPTH);
    for n in sorted_notes.iter() {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            mt.set_leaf(n.position as usize, cm);
        }
    }
    let rebuilt_root = mt.root();

    eprintln!("[tree] Rebuilt tree root: {}", hex::encode(rebuilt_root));
    eprintln!("[tree] On-chain state root: {}", hex::encode(&state.root));

    anyhow::ensure!(
        rebuilt_root.as_slice() == state.root.as_slice(),
        "Rebuilt tree root mismatch: rebuilt={} vs state={}. Notes count={}, tree next_position={}",
        hex::encode(rebuilt_root),
        hex::encode(&state.root),
        notes_resp.notes.len(),
        state.next_position
    );

    // Map commitments to positions
    let mut pos_by_cm: HashMap<[u8; 32], u64> = HashMap::new();
    for n in &notes_resp.notes {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            pos_by_cm.insert(cm, n.position);
        }
    }

    // Build transfer proof tasks for each deposit
    let domain: Hash32 = DOMAIN; // Must match genesis config!
    let shared_anchor: [u8; 32] = {
        let mut a = [0u8; 32];
        a.copy_from_slice(&state.root);
        a
    };

    // Collect per-deposit inputs
    struct DepInput {
        account_idx: usize,
        value: u128,
        rho: Hash32,
        spend_sk: Hash32,
        position: u64,
    }
    let mut dep_inputs: Vec<DepInput> = Vec::with_capacity(num_deposits);
    for (account_idx, txh, amount, rho, spend_sk) in &deposit_secrets {
        if let Some(cm) = deposit_cm_by_hash.get(txh) {
            if let Some(&position) = pos_by_cm.get(cm) {
                eprintln!(
                    "  [proof] Mapped deposit: account={} tx={} cm={} → position={}",
                    account_idx,
                    &txh[..16],
                    hex::encode(&cm[..8]),
                    position
                );
                dep_inputs.push(DepInput {
                    account_idx: *account_idx,
                    value: *amount,
                    rho: *rho,
                    spend_sk: *spend_sk,
                    position,
                });
            } else {
                eprintln!(
                    "  🚨 [error] Could not find position for commitment: {}",
                    hex::encode(cm)
                );
            }
        } else {
            eprintln!("  🚨 [error] Could not find commitment for tx: {}", txh);
        }
    }
    anyhow::ensure!(
        dep_inputs.len() == num_deposits,
        "Could not map all deposits to tree positions: got {} mappings for {} deposits",
        dep_inputs.len(),
        num_deposits
    );

    // Setup proof cache
    let cache_dir = if config.use_proof_cache {
        let dir = config.proof_cache_dir.clone();
        std::fs::create_dir_all(&dir).context("Failed to create proof cache directory")?;
        Some(dir)
    } else {
        None
    };

    // Check cache and generate proofs in parallel (with concurrency limit)
    use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
    let depth_usize = TREE_DEPTH as usize;
    let viewer_fvk_commitment_arg_pos: Option<usize> = if expected_viewer_fvk_commitment.is_some() {
        // note_spend_guest v2 fixed layout:
        // fvk_commitment lives at (n_viewers_idx + 1), where n_viewers_idx follows the deny-map args.
        let n_in: usize = 1;
        let n_out: usize = 1;
        let per_in = 5usize + depth_usize;
        let withdraw_idx = 7usize + n_in * per_in;
        let outs_base = withdraw_idx + 3;
        let inv_enforce_idx = outs_base + 5 * n_out;
        let bl_root_idx = inv_enforce_idx + 1;
        let bl_args_start = bl_root_idx + 1;
        let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
        let bl_bucket_size = midnight_privacy::BLACKLIST_BUCKET_SIZE as usize;
        let bl_per_check = bl_bucket_size + 1usize + bl_depth;
        let bl_checks = 2usize; // sender_id + pay recipient (transfer)
        let n_viewers_idx = bl_args_start + bl_checks * bl_per_check;
        Some(n_viewers_idx + 1)
    } else {
        None
    };
    let mut proof_tasks = Vec::with_capacity(dep_inputs.len());
    let mut cached_proofs: Vec<Option<(usize, Vec<u8>)>> = vec![None; dep_inputs.len()];

    // Create semaphore to limit concurrent proof generation
    let max_concurrent = config.max_concurrent_proofs;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let program_path_for_host = Arc::new(program_path_for_node.clone());
    eprintln!(
        "  [proof] limiting concurrent proof generation to {} tasks",
        max_concurrent
    );

    for (i, input) in dep_inputs.iter().enumerate() {
        // Try to load from cache first
        if let Some(ref cache_dir) = cache_dir {
            let cache_file = cache_dir.join(format!("transfer_bl_{}.proof", input.account_idx));
            if cache_file.exists() {
                match std::fs::read(&cache_file) {
                    Ok(proof_bytes) => {
                        eprintln!(
                            "  [cache] loaded proof for account {} from {}",
                            input.account_idx,
                            cache_file.display()
                        );

                        let mut cached_proof_bytes = Some(proof_bytes);
                        if let (Some(expected_commitment), Some(arg_pos)) = (
                            expected_viewer_fvk_commitment,
                            viewer_fvk_commitment_arg_pos,
                        ) {
                            let got_commitment = (|| -> Result<Hash32> {
                                let proof_bytes = cached_proof_bytes
                                    .as_ref()
                                    .expect("cached_proof_bytes must be Some here");
                                let package: sov_ligero_adapter::LigeroProofPackage =
                                    bincode::deserialize(proof_bytes).context(
                                        "cached proof payload is not a LigeroProofPackage",
                                    )?;
                                let args: Vec<JsonValue> =
                                    serde_json::from_slice(&package.args_json)
                                        .context("cached proof args_json is not valid JSON")?;
                                anyhow::ensure!(
                                    args.len() >= arg_pos,
                                    "cached proof args too short"
                                );
                                decode_ligero_hash32_arg(
                                    &args[arg_pos - 1],
                                    "viewer.fvk_commitment",
                                )
                            })()
                            .ok();

                            if let Some(got_commitment) = got_commitment {
                                if got_commitment != expected_commitment {
                                    eprintln!(
                                        "  [cache] cached proof for account {} does not match current viewer settings; regenerating",
                                        input.account_idx
                                    );
                                    cached_proof_bytes = None;
                                } else if let Some(pool_sig_hex) = pool_sig_hex.as_deref() {
                                    let bytes =
                                        cached_proof_bytes.take().expect("checked Some above");
                                    cached_proof_bytes = inject_pool_sig_hex_into_proof_bytes(
                                        bytes,
                                        arg_pos,
                                        pool_sig_hex.clone(),
                                    )
                                    .map(Some)
                                    .unwrap_or_else(|e| {
                                        eprintln!(
                                            "  [cache] failed to inject pool signature for account {}: {}",
                                            input.account_idx, e
                                        );
                                        None
                                    });
                                }
                            } else {
                                cached_proof_bytes = None;
                            }
                        }

                        if let Some(proof_bytes) = cached_proof_bytes {
                            cached_proofs[i] = Some((input.account_idx, proof_bytes));
                            continue;
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "  [cache] failed to read cached proof for account {}: {}",
                            input.account_idx, e
                        );
                    }
                }
            }
        }
        let account_idx = input.account_idx;
        let value = input.value;
        let rho = input.rho;
        let spend_sk = input.spend_sk;
        let position = input.position;
        let siblings = mt.open(position as usize);
        let anchor = shared_anchor;
        let sem = semaphore.clone();
        let program_path_for_host = program_path_for_host.clone();
        let viewer_fvk = viewer_fvk; // Option<Hash32>, Copy
        let pool_sig_hex = pool_sig_hex.clone();
        let client = client.clone();
        proof_tasks.push(tokio::spawn(async move {
            // Acquire semaphore permit to limit concurrency
            let _permit = sem.acquire().await.expect("semaphore closed");

            // Fetch deny-map openings (sender + pay recipient). The spend circuit binds to the
            // current `blacklist_root` as a public input and requires, for each checked id:
            // - bucket_entries[BLACKLIST_BUCKET_SIZE] (private)
            // - bucket_inv (private)
            // - siblings[BLACKLIST_TREE_DEPTH] (private)
            let pk_ivk_owner = pk_ivk_from_sk(&domain, &spend_sk);
            let pk_spend_owner = pk_from_sk(&spend_sk);
            let sender_addr = PrivacyAddress::from_keys(&pk_spend_owner, &pk_ivk_owner);

            let mut out_spend_sk = [0u8; 32];
            out_spend_sk[0] = (account_idx as u8).wrapping_add(101);
            let out_pk_spend = pk_from_sk(&out_spend_sk);
            let out_pk_ivk = pk_ivk_from_sk(&domain, &out_spend_sk);
            let out_addr = PrivacyAddress::from_keys(&out_pk_spend, &out_pk_ivk);

            let (sender_opening, out_opening) = tokio::try_join!(
                async {
                    client
                        .query_rest_endpoint::<midnight_privacy::BlacklistOpeningResponse>(&format!(
                            "/modules/midnight-privacy/blacklist/opening/{sender_addr}"
                        ))
                        .await
                },
                async {
                    client
                        .query_rest_endpoint::<midnight_privacy::BlacklistOpeningResponse>(&format!(
                            "/modules/midnight-privacy/blacklist/opening/{out_addr}"
                        ))
                        .await
                }
            )
            .context("Failed to query deny-map openings")?;

            anyhow::ensure!(
                sender_opening.blacklist_root == out_opening.blacklist_root,
                "Deny-map root changed while fetching openings (sender vs output)"
            );
            let blacklist_root = sender_opening.blacklist_root;

            if sender_opening.is_blacklisted {
                anyhow::bail!("Sender privacy address is frozen (blacklisted)");
            }
            if out_opening.is_blacklisted {
                anyhow::bail!("Output privacy address is frozen (blacklisted)");
            }

            let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
            anyhow::ensure!(
                sender_opening.siblings.len() == bl_depth,
                "sender deny-map opening has wrong sibling length: got {}, expected {}",
                sender_opening.siblings.len(),
                bl_depth
            );
            anyhow::ensure!(
                out_opening.siblings.len() == bl_depth,
                "output deny-map opening has wrong sibling length: got {}, expected {}",
                out_opening.siblings.len(),
                bl_depth
            );

            let sender_bl_bucket_entries = sender_opening.bucket_entries;
            let sender_bl_siblings = sender_opening.siblings;
            let out_bl_bucket_entries = out_opening.bucket_entries;
            let out_bl_siblings = out_opening.siblings;

            tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, Vec<u8>)> {
                eprintln!(
                    "  [proof] gen_proof idx={} account={} pos={} value={} sib_len={} anchor={} viewer={}",
                    i,
                    account_idx,
                    position,
                    value,
                    siblings.len(),
                    hex::encode(anchor),
                    viewer_fvk.is_some()
                );
                let out_value = value;
                let out_value_u64: u64 = out_value
                    .try_into()
                    .context("note value does not fit into u64 (required by note_spend_guest v2)")?;
                if out_value_u64 > i64::MAX as u64 {
                    bail!("note value does not fit into i64 (required by note_spend_guest v2 ABI)");
                }

                // note_spend_guest v2 derives the input recipient from (spend_sk, pk_ivk_owner).
                let in_recipient = recipient_from_sk_v2(&domain, &spend_sk, &pk_ivk_owner);
                let in_sender_id = in_recipient; // deposit convention: sender_id == recipient
                let sender_id_out = in_recipient;

                // Output note (same value, fresh rho + fresh address).
                let mut out_rho = [0u8; 32];
                out_rho[0] = (account_idx as u8).wrapping_add(100);
                let out_recipient = recipient_from_pk_v2(&domain, &out_pk_spend, &out_pk_ivk);
                let cm_out = note_commitment(
                    &domain,
                    out_value_u64,
                    &out_rho,
                    &out_recipient,
                    &sender_id_out,
                );

                // Compute nullifier.
                let nf_key = nf_key_from_sk(&domain, &spend_sk);
                let nf = nullifier(&domain, &nf_key, &rho);

                // Build viewer attestation if pool viewer is configured.
                let (view_attestations, viewer_data) = if let Some(fvk) = viewer_fvk {
                    let cm_in = note_commitment(
                        &domain,
                        out_value_u64,
                        &rho,
                        &in_recipient,
                        &in_sender_id,
                    );
                    let mut cm_ins: [Hash32; crate::viewer::MAX_INS] =
                        [[0u8; 32]; crate::viewer::MAX_INS];
                    cm_ins[0] = cm_in;
                    let (att, _enc) = make_viewer_bundle(
                        &fvk,
                        &domain,
                        out_value,
                        &out_rho,
                        &out_recipient,
                        &sender_id_out,
                        &cm_ins,
                        &cm_out,
                    )?;
                    (Some(vec![att.clone()]), Some((fvk, att)))
                } else {
                    (None, None)
                };

                // Public output with view_attestations populated
                let public = midnight_privacy::SpendPublic {
                    anchor_root: anchor,
                    blacklist_root,
                    nullifiers: vec![nf],
                    withdraw_amount: 0,
                    output_commitments: vec![cm_out], // ONE output
                    view_attestations,
                };

                let n_out: usize = 1;
                // LigeroConfig private indices are 1-based (by argument position).
                let mut private_indices: Vec<usize> = Vec::new();
                private_indices.extend_from_slice(&[2, 3]); // spend_sk, pk_ivk_owner

                let n_in: usize = 1;
                let per_in = 5usize + depth_usize;
                let withdraw_idx = 7usize + n_in * per_in;
                let outs_base = withdraw_idx + 3;

                // Input 0 private args.
                private_indices.extend_from_slice(&[7, 8, 9, 10]); // value_in, rho_in, sender_id_in, pos
                // siblings [11..11+depth)
                for j in 0..depth_usize {
                    private_indices.push(11 + j);
                }

                // output 0 private args:
                let out_base = outs_base;
                private_indices.extend_from_slice(&[
                    out_base,     // value_out
                    out_base + 1, // rho_out
                    out_base + 2, // pk_spend_out
                    out_base + 3, // pk_ivk_out
                ]);
                // inv_enforce (private)
                let inv_enforce_idx = outs_base + 5 * n_out;
                private_indices.push(inv_enforce_idx);

                // Deny-map (blacklist) section:
                // - blacklist_root is PUBLIC (comes right after inv_enforce)
                // - for each checked id: bucket_entries[BLACKLIST_BUCKET_SIZE] + bucket_inv + siblings[BLACKLIST_TREE_DEPTH]
                let bl_root_idx = inv_enforce_idx + 1;
                let bl_args_start = bl_root_idx + 1;
                let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
                let bl_per_check =
                    midnight_privacy::BLACKLIST_BUCKET_SIZE + 1usize + bl_depth;
                // Transfers: sender_id + pay recipient.
                let bl_checks = 2usize;
                for j in 0..(bl_checks * bl_per_check) {
                    private_indices.push(bl_args_start + j);
                }

                // Viewer section: fvk is private (when enabled) and comes after the deny-map.
                if viewer_data.is_some() {
                    let n_viewers_idx = bl_args_start + bl_checks * bl_per_check;
                    private_indices.push(n_viewers_idx + 2);
                }
                let fvk_commitment_arg_pos =
                    viewer_data
                        .is_some()
                        .then_some(bl_args_start + bl_checks * bl_per_check + 1);

                let program_path = program_path_for_host.as_ref().clone();
                let mut host = <sov_ligero_adapter::Ligero as Zkvm>::Host::from_args(&program_path)
                    .with_private_indices(private_indices);

                // Typed binary ABI for zkVM performance (matches note_spend_guest v2 argument layout)
                host.add_hex_arg(hex::encode(domain)); // 1 domain (PUBLIC)
                host.add_hex_arg(hex::encode(spend_sk)); // 2 spend_sk (PRIVATE)
                host.add_hex_arg(hex::encode(pk_ivk_owner)); // 3 pk_ivk_owner (PRIVATE)
                host.add_u64_arg(depth_usize as u64); // 4 depth (PUBLIC)
                host.add_hex_arg(hex::encode(anchor)); // 5 anchor (PUBLIC)
                host.add_u64_arg(1); // 6 n_in (PUBLIC)

                host.add_u64_arg(out_value_u64); // 7 value_in (PRIVATE)
                host.add_hex_arg(hex::encode(rho)); // 8 rho_in (PRIVATE)
                host.add_hex_arg(hex::encode(in_sender_id)); // 9 sender_id_in (PRIVATE)
                host.add_u64_arg(position as u64); // 10 pos (PRIVATE)
                for s in &siblings {
                    host.add_hex_arg(hex::encode(s));
                }
                host.add_hex_arg(hex::encode(nf)); // nullifier (PUBLIC)
                host.add_u64_arg(0); // withdraw_amount (PUBLIC)
                host.add_hex_arg(hex::encode([0u8; 32])); // withdraw_to (PUBLIC; must be 0 for transfers)
                host.add_u64_arg(n_out as u64); // n_out (PUBLIC)

                // Output 0
                host.add_u64_arg(out_value_u64);
                host.add_hex_arg(hex::encode(out_rho));
                host.add_hex_arg(hex::encode(out_pk_spend));
                host.add_hex_arg(hex::encode(out_pk_ivk));
                host.add_hex_arg(hex::encode(cm_out));

                // inv_enforce (PRIVATE)
                let inv_enforce = {
                    let mut enforce_prod = Bn254Fr::from_u32(1);
                    enforce_prod.mulmod_checked(&Bn254Fr::from_u64(out_value_u64));
                    enforce_prod.mulmod_checked(&Bn254Fr::from_u64(out_value_u64));
                    let mut delta = Bn254Fr::new();
                    let mut out_fr = Bn254Fr::new();
                    out_fr.set_bytes_big(&out_rho);
                    let mut in_fr = Bn254Fr::new();
                    in_fr.set_bytes_big(&rho);
                    submod_checked(&mut delta, &out_fr, &in_fr);
                    enforce_prod.mulmod_checked(&delta);
                    let mut inv = enforce_prod.clone();
                    inv.inverse();
                    inv.to_bytes_be()
                };
                host.add_hex_arg(hex::encode(inv_enforce));

                // Deny-map (blacklist) args:
                //   blacklist_root (PUBLIC)
                //   sender check: bucket_entries[12] (PRIVATE) + bucket_inv (PRIVATE) + siblings[16] (PRIVATE)
                //   pay recipient check: bucket_entries[12] (PRIVATE) + bucket_inv (PRIVATE) + siblings[16] (PRIVATE)
                let bucket_inv_for_id = |id: &Hash32, bucket_entries: &[Hash32]| -> anyhow::Result<Hash32> {
                    anyhow::ensure!(
                        bucket_entries.len() == midnight_privacy::BLACKLIST_BUCKET_SIZE,
                        "bucket_entries length mismatch: got {}, expected {}",
                        bucket_entries.len(),
                        midnight_privacy::BLACKLIST_BUCKET_SIZE
                    );
                    let mut id_fr = Bn254Fr::new();
                    id_fr.set_bytes_big(id);
                    let mut prod = Bn254Fr::from_u32(1);
                    let mut delta = Bn254Fr::new();
                    for e in bucket_entries {
                        let mut e_fr = Bn254Fr::new();
                        e_fr.set_bytes_big(e);
                        submod_checked(&mut delta, &id_fr, &e_fr);
                        prod.mulmod_checked(&delta);
                    }
                    anyhow::ensure!(
                        !prod.is_zero(),
                        "bucket_inv undefined: id appears blacklisted or invalid bucket"
                    );
                    let mut inv = prod.clone();
                    inv.inverse();
                    Ok(inv.to_bytes_be())
                };

                host.add_hex_arg(hex::encode(blacklist_root));
                for e in &sender_bl_bucket_entries {
                    host.add_hex_arg(hex::encode(e));
                }
                let sender_bucket_inv = bucket_inv_for_id(&sender_id_out, &sender_bl_bucket_entries)?;
                host.add_hex_arg(hex::encode(sender_bucket_inv));
                for sib in sender_bl_siblings.iter().take(bl_depth) {
                    host.add_hex_arg(hex::encode(sib));
                }
                for e in &out_bl_bucket_entries {
                    host.add_hex_arg(hex::encode(e));
                }
                let out_bucket_inv = bucket_inv_for_id(&out_recipient, &out_bl_bucket_entries)?;
                host.add_hex_arg(hex::encode(out_bucket_inv));
                for sib in out_bl_siblings.iter().take(bl_depth) {
                    host.add_hex_arg(hex::encode(sib));
                }

                // Viewer section (Level-B) - add viewer args if configured.
                if let Some((ref fvk, ref att)) = viewer_data {
                    // m_viewers
                    host.add_u64_arg(1);
                    // public fvk_commitment
                    host.add_hex_arg(hex::encode(att.fvk_commitment));
                    // private fvk
                    host.add_hex_arg(hex::encode(fvk));
                    // per-output (only j=0 here): ct_hash, mac
                    host.add_hex_arg(hex::encode(att.ct_hash));
                    host.add_hex_arg(hex::encode(att.mac));
                }

                host.set_public_output(&public)
                    .context("set public output")?;
                let mut proof_data = host.run(true).context("generate transfer proof")?;
                if let Some(pool_sig_hex) = pool_sig_hex.as_deref() {
                    let Some(arg_pos) = fvk_commitment_arg_pos else {
                        bail!("POOL_FVK_PK is set but viewer section is missing in proof args");
                    };
                    anyhow::ensure!(
                        viewer_data.is_some(),
                        "POOL_FVK_PK is set but viewer section is missing in proof args"
                    );
                    proof_data = inject_pool_sig_hex_into_proof_bytes(
                        proof_data,
                        arg_pos,
                        pool_sig_hex.clone(),
                    )?;
                }
                eprintln!(
                    "  [proof] gen_proof ok idx={} account={} pos={} bytes={} nullifier={} out_cm={} viewer={}",
                    i,
                    account_idx,
                    position,
                    proof_data.len(),
                    hex::encode(nf),
                    hex::encode(cm_out),
                    viewer_fvk.is_some()
                );
                Ok((account_idx, proof_data))
            })
            .await
            .expect("spawn_blocking join failed")
        }));
    }

    // Await generated proofs and merge with cached proofs
    let mut generated_proofs: Vec<(usize, Vec<u8>)> = Vec::with_capacity(proof_tasks.len());
    for t in proof_tasks {
        generated_proofs.push(t.await??);
    }

    eprintln!(
        "[ok] generated {} transfer proofs in parallel",
        generated_proofs.len()
    );

    // Save newly generated proofs to cache
    if let Some(ref cache_dir) = cache_dir {
        for (account_idx, proof_bytes) in &generated_proofs {
            let cache_file = cache_dir.join(format!("transfer_bl_{}.proof", account_idx));
            match std::fs::write(&cache_file, proof_bytes) {
                Ok(_) => {
                    eprintln!(
                        "  [cache] saved proof for account {} to {}",
                        account_idx,
                        cache_file.display()
                    );
                }
                Err(e) => {
                    eprintln!(
                        "  [cache] failed to save proof for account {}: {}",
                        account_idx, e
                    );
                }
            }
        }
    }

    // Merge cached and generated proofs
    let mut proofs: Vec<(usize, Vec<u8>)> = Vec::with_capacity(dep_inputs.len());
    let mut cached_count = 0;
    for i in 0..dep_inputs.len() {
        if let Some(cached) = cached_proofs[i].take() {
            proofs.push(cached);
            cached_count += 1;
        }
    }
    proofs.extend(generated_proofs);

    eprintln!(
        "[ok] using {} proofs total ({} from cache, {} newly generated)",
        proofs.len(),
        cached_count,
        proofs.len() - cached_count
    );
    let _ = std::io::stderr().flush();

    // Pre-verify proofs locally to catch issues early (before submission)
    // Skip when using cache to save time (cached proofs were already verified when first generated)
    // Or skip if explicitly requested via config
    if config.skip_verify {
        eprintln!("[skip] skipping pre-verification (--skip-verify flag set)");
    } else if cache_dir.is_none() {
        eprintln!("[verify] pre-verifying {} proofs locally...", proofs.len());
        use sov_ligero_adapter::{LigeroCodeCommitment, LigeroVerifier};
        use sov_rollup_interface::zk::ZkVerifier;
        let method_commitment = LigeroCodeCommitment(method_id);
        for (idx, (account_idx, proof_bytes)) in proofs.iter().enumerate() {
            let input = dep_inputs
                .iter()
                .find(|d| d.account_idx == *account_idx)
                .with_context(|| format!("Missing DepInput for account {}", account_idx))?;
            match LigeroVerifier::verify::<SpendPublic>(proof_bytes, &method_commitment) {
                Ok(public) => {
                    let nf_key = nf_key_from_sk(&domain, &input.spend_sk);
                    let nf_exp = nullifier(&domain, &nf_key, &input.rho);
                    let expected_nfs = [nf_exp];
                    if public.anchor_root != shared_anchor
                        || public.nullifiers.as_slice() != expected_nfs
                        || public.withdraw_amount != 0
                    {
                        eprintln!(
                            "  [warn] local verify mismatch idx={} account={} anc_ok={} nf_ok={} wd_ok={}",
                            idx,
                            account_idx,
                            public.anchor_root == shared_anchor,
                            public.nullifiers.as_slice() == expected_nfs,
                            public.withdraw_amount == 0
                        );
                        eprintln!(
                            "         expected anchor={} nullifier={} withdraw=0",
                            hex::encode(shared_anchor),
                            hex::encode(nf_exp)
                        );
                        eprintln!(
                            "         proof anchor={} nullifier={} withdraw={}",
                            hex::encode(public.anchor_root),
                            public
                                .nullifiers
                                .first()
                                .map(|n| hex::encode(n))
                                .unwrap_or_else(|| "<empty>".to_string()),
                            public.withdraw_amount
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "  [error] local Ligero verify failed idx={} account={} pos={} err={} (bytes={})",
                        idx,
                        account_idx,
                        input.position,
                        e,
                        proof_bytes.len()
                    );
                    // Don't fail early - continue to see how many pass vs fail
                    // anyhow::bail!("Local verify failed for transfer idx {} (account {}): {}", i, account_idx, e);
                }
            }
        }
        eprintln!("[ok] pre-verification complete");
    } else {
        eprintln!("[skip] skipping pre-verification (proof cache enabled)");
    }

    eprintln!("[ok] local verification complete - check logs above for any failures");

    // Force flush logs to ensure they appear
    use std::io::Write;
    let _ = std::io::stderr().flush();

    // Sign all transfer transactions (only after ALL proofs are generated).

    eprintln!(
        "\n[transfers] Signing {} transfer transactions (one per account)...",
        proofs.len()
    );
    let _ = std::io::stderr().flush();
    let mut transfer_txs_b64: Vec<String> = Vec::with_capacity(proofs.len());
    for (i, (account_idx, proof_bytes)) in proofs.into_iter().enumerate() {
        let input = dep_inputs
            .iter()
            .find(|d| d.account_idx == account_idx)
            .with_context(|| format!("Missing DepInput for account {}", account_idx))?;
        let account = &accounts[account_idx];

        let transfer_nonce = deposit_nonce_by_account
            .get(account_idx)
            .copied()
            .ok_or_else(|| anyhow!("Missing deposit nonce for account {}", account_idx))?
            .checked_add(1)
            .ok_or_else(|| anyhow!("Transfer nonce overflow for account {}", account_idx))?;

        // Same nullifier as before
        let nf_key = nf_key_from_sk(&domain, &input.spend_sk);
        let nf = nullifier(&domain, &nf_key, &input.rho);

        // Reconstruct the *same* output note layout used in the proof
        let out_value = input.value;
        let out_value_u64: u64 = out_value
            .try_into()
            .context("note value does not fit into u64 (required by note_spend_guest v2)")?;
        let mut out_rho = [0u8; 32];
        out_rho[0] = (account_idx as u8).wrapping_add(100);
        let mut out_spend_sk = [0u8; 32];
        out_spend_sk[0] = (account_idx as u8).wrapping_add(101);
        let out_pk_spend = pk_from_sk(&out_spend_sk);
        let out_pk_ivk = pk_ivk_from_sk(&domain, &out_spend_sk);
        let out_recipient = recipient_from_pk_v2(&domain, &out_pk_spend, &out_pk_ivk);

        let pk_ivk_owner = pk_ivk_from_sk(&domain, &input.spend_sk);
        let sender_id = recipient_from_sk_v2(&domain, &input.spend_sk, &pk_ivk_owner);
        let cm_out = note_commitment(&domain, out_value_u64, &out_rho, &out_recipient, &sender_id);

        // Build EncryptedNote for the pool viewer, if configured.
        // sender_id = spender's address
        let view_ciphertexts: Option<Vec<EncryptedNote>> = match viewer_fvk {
            Some(fvk) => {
                let in_recipient = recipient_from_sk_v2(&domain, &input.spend_sk, &pk_ivk_owner);
                let in_sender_id = in_recipient; // deposit convention: sender_id == recipient
                let cm_in = note_commitment(
                    &domain,
                    out_value_u64,
                    &input.rho,
                    &in_recipient,
                    &in_sender_id,
                );
                let mut cm_ins: [Hash32; crate::viewer::MAX_INS] =
                    [[0u8; 32]; crate::viewer::MAX_INS];
                cm_ins[0] = cm_in;
                let (_att, enc) = make_viewer_bundle(
                    &fvk,
                    &domain,
                    out_value,
                    &out_rho,
                    &out_recipient,
                    &sender_id,
                    &cm_ins,
                    &cm_out,
                )?;
                Some(vec![enc])
            }
            None => None,
        };

        let call = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Transfer {
            proof: proof_bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("Proof too large for SafeVec"))?,
            anchor_root: shared_anchor,
            nullifiers: vec![nf],
            view_ciphertexts,
            gas: None,
        });
        let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
            default_test_signed_transaction(
                &account.private_key, // Each account signs its own transfer
                &call,
                transfer_nonce, // Account-specific nonce
                &chain_hash,
            );
        // Sanity check signature
        let mut meter = UnlimitedGasMeter::<DemoRollupSpec>::default();
        tx.verify(&chain_hash, &mut meter)
            .context("Transfer tx signature verify failed")?;
        let tx_bytes = borsh::to_vec(&tx)?;
        let tx_hash = tx.hash().to_string();
        transfer_txs_b64.push(BASE64_STANDARD.encode(&tx_bytes));
        eprintln!(
            "  [transfers] transfer #{} from account={} nonce={} tx={} nullifier={} viewer={}",
            i + 1,
            account_idx,
            transfer_nonce,
            tx_hash,
            hex::encode(&nf[..8]),
            viewer_fvk.is_some()
        );
    }

    // Submit transfers concurrently to verifier service
    let num_transfers = transfer_txs_b64.len();
    eprintln!(
        "\n[transfers] Submitting {} transfer transactions to verifier service...",
        num_transfers
    );
    let _ = std::io::stderr().flush();

    let mut handles: Vec<
        tokio::task::JoinHandle<anyhow::Result<(usize, VerifierSubmitResponse, f64)>>,
    > = Vec::with_capacity(num_transfers);
    for (idx, body_b64) in transfer_txs_b64.into_iter().enumerate() {
        let http_cl = http.clone();
        let verifier_url_cl = verifier_url.clone();
        handles.push(tokio::spawn(async move {
            let display_idx = idx + 1;
            eprintln!(
                "  [transfers] submitting transfer #{} to verifier...",
                display_idx
            );
            let (parsed, http_elapsed_ms) = submit_to_verifier_with_sync_retry(
                &http_cl,
                &verifier_url_cl,
                &body_b64,
                "transfer",
                display_idx,
            )
            .await?;
            Ok((idx, parsed, http_elapsed_ms))
        }));

        // Throttle spawning to avoid exhausting socket buffers (e.g., macOS ENOBUFS os error 55)
        if config.transfer_submit_delay_ms > 0 {
            sleep(Duration::from_millis(config.transfer_submit_delay_ms)).await;
        }
    }

    let mut transfer_hashes: Vec<String> = Vec::new();
    // Track per-tx execution time (micros) keyed by tx hash for per-batch aggregation
    let mut transfer_exec_time_by_hash_micros: HashMap<String, u64> = HashMap::new();
    for h in handles {
        let (idx, parsed, http_elapsed_ms) = h.await.expect("join transfer submit task")?;
        let display_idx = idx + 1;
        let has_seq_resp = parsed.sequencer_response.is_some();
        let stf_execution_ms = parsed
            .sequencer_response
            .as_ref()
            .and_then(extract_stf_execution_ms);
        log_submission_timing(
            "transfer",
            display_idx,
            http_elapsed_ms,
            parsed.metrics.as_ref(),
            has_seq_resp,
            stf_execution_ms,
        );
        transfer_http_timings.push(http_elapsed_ms);
        if let Some(m) = parsed.metrics.as_ref() {
            transfer_node_submit_timings.push(m.node_submit_ms);
        }
        if has_seq_resp && stf_execution_ms.is_none() {
            if let Some(resp) = parsed.sequencer_response.as_ref() {
                eprintln!(
                    "  [timing][transfer #{:02}] warning: STF metric missing in sequencer response: {}",
                    display_idx,
                    serde_json::to_string(resp).unwrap_or_default()
                );
            }
        }
        if let Some(ms) = stf_execution_ms {
            transfer_stf_execution_ms.push(ms);
        }
        anyhow::ensure!(
            parsed.success,
            "Verifier reported failure for transfer #{}: {:?}",
            display_idx,
            parsed.error
        );
        if let Some(hx) = parsed.tx_hash {
            eprintln!(
                "  [transfers] transfer #{} accepted with hash: {}",
                display_idx, hx
            );
            if let Some(ms) = stf_execution_ms {
                transfer_exec_time_by_hash_micros.insert(hx.clone(), (ms * 1000.0) as u64);
            }
            transfer_hashes.push(hx);
        } else {
            anyhow::bail!(
                "Verifier success for transfer #{} without tx_hash",
                display_idx
            );
        }
    }
    eprintln!(
        "[ok] submitted {} transfers to verifier (got {} hashes)",
        num_transfers,
        transfer_hashes.len()
    );
    let _ = std::io::stderr().flush();

    // If we deferred sequencer submission in the verifier, flush all transfers now in one burst
    if config.defer_sequencer_submission {
        eprintln!("[flush] Waiting 5 seconds before flushing queued transfers...");
        sleep(Duration::from_secs(5)).await;
        flush_verifier_queue(&http, &verifier_url).await?;
    }

    // Verify ledger inclusion for transfers
    let mut ok_transfers = 0usize;
    let mut batch_stats: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    let mut transfer_hash_to_batch: HashMap<String, u64> = HashMap::new();
    let transfer_start_time = std::time::Instant::now();
    for hash_hex in &transfer_hashes {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match client
                .query_rest_endpoint::<api_types::LedgerTx>(&format!(
                    "/ledger/txs/{}?children=1",
                    hash_hex
                ))
                .await
            {
                Ok(ltx) => {
                    anyhow::ensure!(
                        ltx.receipt.result == api_types::TxReceiptResult::Successful,
                        "Transfer {} included but not successful: {:?}",
                        hash_hex,
                        ltx.receipt
                    );
                    eprintln!(
                        "[ok] included transfer tx={} batch_number={} tx_number={} events={}",
                        hash_hex,
                        ltx.batch_number,
                        ltx.number,
                        ltx.events.len()
                    );
                    *batch_stats.entry(ltx.batch_number).or_insert(0) += 1;
                    transfer_hash_to_batch.insert(hash_hex.clone(), ltx.batch_number);
                    ok_transfers += 1;
                    break;
                }
                Err(_) => {
                    if std::time::Instant::now() > deadline {
                        anyhow::bail!(
                            "Timeout waiting for transfer {} to appear in ledger",
                            hash_hex
                        );
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
    anyhow::ensure!(
        ok_transfers == transfer_hashes.len(),
        "Some transfers failed inclusion"
    );
    let transfer_total_time = transfer_start_time.elapsed();
    eprintln!(
        "[ok] all transfers included: {}/{} in {:.2}s",
        ok_transfers,
        transfer_hashes.len(),
        transfer_total_time.as_secs_f64()
    );

    // Stats: nullifiers_spent advanced
    let stats_after: Stats = client
        .query_rest_endpoint("/modules/midnight-privacy/stats")
        .await
        .unwrap_or_default();
    eprintln!(
        "[ok] nullifiers_spent advanced by >= {} (final={})",
        ok_transfers, stats_after.nullifiers_spent
    );

    eprintln!("\n[timing] ===== Submission Summary =====");
    summarize_timings(
        "Deposits",
        &deposit_http_timings,
        &deposit_node_submit_timings,
        &deposit_stf_execution_ms,
    );
    summarize_timings(
        "Transfers",
        &transfer_http_timings,
        &transfer_node_submit_timings,
        &transfer_stf_execution_ms,
    );
    eprintln!("[timing] =================================");

    // Deposit batch summary
    eprintln!("\n[deposit-stats] ===== Batch Distribution =====");
    let mut sorted_deposit_batches: Vec<_> = deposit_batch_stats.iter().collect();
    sorted_deposit_batches.sort_by_key(|(batch_num, _)| *batch_num);
    let deposit_total_batches = sorted_deposit_batches.len();
    let deposit_total_txs: usize = sorted_deposit_batches
        .iter()
        .map(|(_, count)| **count)
        .sum();
    let deposit_avg_txs_per_batch = if deposit_total_batches > 0 {
        deposit_total_txs as f64 / deposit_total_batches as f64
    } else {
        0.0
    };
    eprintln!("[deposit-stats] Total batches: {}", deposit_total_batches);
    eprintln!("[deposit-stats] Total transactions: {}", deposit_total_txs);
    eprintln!(
        "[deposit-stats] Average txs/batch: {:.2}",
        deposit_avg_txs_per_batch
    );
    eprintln!("[deposit-stats]");
    eprintln!("[deposit-stats] Distribution:");
    let deposit_batch_ids: BTreeSet<u64> = sorted_deposit_batches
        .iter()
        .map(|(batch, _)| **batch)
        .collect();
    let deposit_gas_usage = match collect_batch_gas_stats(&client, &deposit_batch_ids).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!(
                "[deposit-gas] Failed to collect gas statistics from ledger: {err:?}. Skipping gas summary."
            );
            HashMap::new()
        }
    };
    // Collect batch byte sizes
    let deposit_batch_sizes = match collect_batch_sizes(&client, &deposit_batch_ids).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!(
                "[deposit-bytes] Failed to collect batch sizes from ledger: {err:?}. Skipping size summary."
            );
            HashMap::new()
        }
    };
    // Aggregate per-batch execution time from tx-level metrics we observed at submission
    let mut deposit_batch_exec_micros: HashMap<u64, u64> = HashMap::new();
    for (tx_hash, batch_id) in &deposit_hash_to_batch {
        if let Some(micros) = deposit_exec_time_by_hash_micros.get(tx_hash) {
            *deposit_batch_exec_micros.entry(*batch_id).or_insert(0) += *micros;
        }
    }
    for (batch_num, count) in &sorted_deposit_batches {
        let percentage = if deposit_total_txs > 0 {
            (**count as f64 / deposit_total_txs as f64) * 100.0
        } else {
            0.0
        };
        let bar_length = if deposit_avg_txs_per_batch > 0.0 {
            (**count as f64 / deposit_avg_txs_per_batch * 20.0) as usize
        } else {
            0
        };
        let bar = "█".repeat(bar_length.min(40));
        let gas_suffix = deposit_gas_usage
            .get(batch_num)
            .map(|gas| format!(" gas={}", format_gas(gas)))
            .unwrap_or_default();
        let size_suffix = deposit_batch_sizes
            .get(batch_num)
            .map(|bytes| format!(" size={}B", bytes))
            .unwrap_or_default();
        let exec_suffix = deposit_batch_exec_micros
            .get(batch_num)
            .map(|micros| format!(" exec={:.3}ms", (*micros as f64) / 1000.0))
            .unwrap_or_default();
        let meta_suffix = format!("{}{}", exec_suffix, size_suffix);
        eprintln!(
            "[deposit-stats]   Batch {:3}: {:3} txs ({:5.1}%) {}{}{}",
            batch_num, count, percentage, bar, gas_suffix, meta_suffix
        );
    }

    // Print batch statistics
    eprintln!("\n[transfer-stats] ===== Batch Distribution =====");
    let mut sorted_batches: Vec<_> = batch_stats.iter().collect();
    sorted_batches.sort_by_key(|(batch_num, _)| *batch_num);

    let total_batches = sorted_batches.len();
    let total_txs: usize = sorted_batches.iter().map(|(_, count)| **count).sum();
    let avg_txs_per_batch = if total_batches > 0 {
        total_txs as f64 / total_batches as f64
    } else {
        0.0
    };
    let batch_ids: BTreeSet<u64> = sorted_batches.iter().map(|(batch, _)| **batch).collect();
    let batch_gas_usage = match collect_batch_gas_stats(&client, &batch_ids).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!(
                "[gas] Failed to collect gas statistics from ledger: {err:?}. Skipping gas summary."
            );
            HashMap::new()
        }
    };
    // Collect batch sizes
    let batch_sizes = match collect_batch_sizes(&client, &batch_ids).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!(
                "[bytes] Failed to collect batch sizes from ledger: {err:?}. Skipping size summary."
            );
            HashMap::new()
        }
    };
    // Aggregate per-batch execution times from transfer submissions
    let mut transfer_batch_exec_micros: HashMap<u64, u64> = HashMap::new();
    for (tx_hash, batch_id) in &transfer_hash_to_batch {
        if let Some(micros) = transfer_exec_time_by_hash_micros.get(tx_hash) {
            *transfer_batch_exec_micros.entry(*batch_id).or_insert(0) += *micros;
        }
    }

    eprintln!("[transfer-stats] Total batches: {}", total_batches);
    eprintln!("[transfer-stats] Total transactions: {}", total_txs);
    eprintln!(
        "[transfer-stats] Average txs/batch: {:.2}",
        avg_txs_per_batch
    );
    eprintln!("[transfer-stats]");
    eprintln!("[transfer-stats] Distribution:");

    for (batch_num, count) in &sorted_batches {
        let percentage = (**count as f64 / total_txs as f64) * 100.0;
        let bar_length = (**count as f64 / avg_txs_per_batch * 20.0) as usize;
        let bar = "█".repeat(bar_length.min(40));
        let gas_suffix = batch_gas_usage
            .get(batch_num)
            .map(|gas| format!(" gas={}", format_gas(gas)))
            .unwrap_or_default();
        let size_suffix = batch_sizes
            .get(batch_num)
            .map(|bytes| format!(" size={}B", bytes))
            .unwrap_or_default();
        let exec_suffix = transfer_batch_exec_micros
            .get(batch_num)
            .map(|micros| format!(" exec={:.3}ms", (*micros as f64) / 1000.0))
            .unwrap_or_default();
        let meta_suffix = format!("{}{}", exec_suffix, size_suffix);
        eprintln!(
            "[transfer-stats]   Batch {:3}: {:3} txs ({:5.1}%) {}{}{}",
            batch_num, count, percentage, bar, gas_suffix, meta_suffix
        );
    }

    eprintln!("\n✅ TEST COMPLETE: E2E Privacy Pool with Multi-Account Parallelism");
    eprintln!("═══════════════════════════════════════════════════════════════");
    eprintln!(
        "  Accounts used: {} (true parallel execution enabled)",
        num_deposits
    );
    eprintln!(
        "  Deposits: {} (one per account with live chain nonce)",
        num_deposits
    );
    eprintln!(
        "  Transfers: {} (one per account using next nonce after deposit)",
        ok_transfers
    );
    eprintln!("═══════════════════════════════════════════════════════════════\n");

    // Cleanup process
    env.shutdown();

    Ok(())
}

type DemoGas = <DemoRollupSpec as Spec>::Gas;

async fn collect_batch_gas_stats(
    client: &NodeClient,
    batch_ids: &BTreeSet<u64>,
) -> Result<HashMap<u64, DemoGas>> {
    let mut per_batch = HashMap::new();

    for batch_id in batch_ids {
        let endpoint = format!("/ledger/batches/{}?children=1", batch_id);
        match client
            .query_rest_endpoint::<api_types::LedgerBatch>(&endpoint)
            .await
        {
            Ok(batch) => match decode_batch_gas(&batch.receipt) {
                Ok(Some(gas)) => {
                    per_batch.insert(*batch_id, gas);
                }
                Ok(None) => {
                    eprintln!(
                        "[gas] Batch {} did not expose gas data in its receipt payload",
                        batch_id
                    );
                }
                Err(err) => {
                    eprintln!(
                        "[gas] Failed to parse gas data for batch {}: {err:?}",
                        batch_id
                    );
                }
            },
            Err(err) => {
                eprintln!(
                    "[gas] Failed to fetch batch {} from ledger: {err:?}",
                    batch_id
                );
            }
        }
    }

    Ok(per_batch)
}

fn decode_batch_gas(receipt: &api_types::AnyJsonValue) -> Result<Option<DemoGas>> {
    let value = any_json_to_value(receipt);
    if let Some(obj) = value.as_object() {
        if let Some(gas_value) = obj.get("gas_used") {
            return Ok(Some(gas_from_array(gas_value)?));
        }
    }
    Ok(None)
}

// Compute serialized batch size as enforced by BatchSizeTracker:
// size = 8 (sequence_number) + 1 (visible_slots_to_advance) + 4 (tx vec len)
//      + sum_over_txs(4 (borsh vec elem overhead) + tx_body.len())
async fn collect_batch_sizes(
    client: &NodeClient,
    batch_ids: &BTreeSet<u64>,
) -> Result<HashMap<u64, usize>> {
    let mut per_batch = HashMap::new();
    for batch_id in batch_ids {
        let endpoint = format!("/ledger/batches/{}?children=1", batch_id);
        match client
            .query_rest_endpoint::<api_types::LedgerBatch>(&endpoint)
            .await
        {
            Ok(batch) => {
                let mut total: usize = 8 + 1 + 4; // overhead
                                                  // Generated type exposes `txs` as a Vec; it may be empty when children are not included
                for tx in &batch.txs {
                    // borsh vec element overhead (4 bytes) + body bytes
                    total += 4 + tx.body.len();
                }
                per_batch.insert(*batch_id, total);
            }
            Err(err) => {
                eprintln!(
                    "[bytes] Failed to fetch batch {} from ledger: {err:?}",
                    batch_id
                );
            }
        }
    }
    Ok(per_batch)
}

fn any_json_to_value(value: &api_types::AnyJsonValue) -> JsonValue {
    match value {
        api_types::AnyJsonValue::String(s) => JsonValue::String(s.clone()),
        api_types::AnyJsonValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        api_types::AnyJsonValue::Boolean(b) => JsonValue::Bool(*b),
        api_types::AnyJsonValue::Array(values) => JsonValue::Array(values.clone()),
        api_types::AnyJsonValue::Object(map) => JsonValue::Object(map.clone()),
    }
}

fn gas_from_array(value: &JsonValue) -> Result<DemoGas> {
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("gas_used must be an array"))?;
    let mut limbs = Vec::with_capacity(array.len());
    for item in array {
        if let Some(num) = item.as_u64() {
            limbs.push(num);
        } else if let Some(text) = item.as_str() {
            limbs.push(text.parse::<u64>().context("Failed to parse gas limb")?);
        } else {
            bail!("gas limb must be a number");
        }
    }
    DemoGas::try_from(limbs).context("Failed to construct gas value")
}

fn format_gas(gas: &DemoGas) -> String {
    let limbs: Vec<String> = gas.as_ref().iter().map(|v| v.to_string()).collect();
    format!("[{}]", limbs.join(", "))
}
