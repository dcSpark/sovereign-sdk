use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use demo_stf::runtime::{Runtime, RuntimeCall};
use ligetron::bn254fr_native::submod_checked;
use ligetron::Bn254Fr;
use midnight_privacy::{
    nf_key_from_sk, note_commitment, nullifier, pk_from_sk, pk_ivk_from_sk, recipient_from_pk_v2,
    recipient_from_sk_v2, CallMessage as MidnightCallMessage, EncryptedNote, Hash32, MerkleTree,
    SpendPublic,
};
use rand::Rng;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::json;
use sov_api_spec::types as api_types;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_node_client::NodeClient;
use sov_rollup_interface::crypto::{PrivateKey as _, PublicKey as _};
use sov_rollup_ligero::MockDemoRollup;
use sov_test_utils::default_test_signed_transaction;
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;
use toml::Value as TomlValue;

use crate::{
    find_rollup_binary, load_authority_fvk, make_viewer_bundle, setup_ligero_env,
    start_local_verifier, wait_for_ready, ChildGuard, LigeroEnv,
};

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

const TREE_DEPTH: u8 = 16;
const TREE_REBUILD_MAX_RETRIES: usize = 5;
const TREE_REBUILD_RETRY_DELAY_MS: u64 = 500;
const MISSING_NOTE_RETRY_MAX: usize = 10;
const MISSING_NOTE_RETRY_DELAY_MS: u64 = 300;
const DOMAIN: [u8; 32] = [1u8; 32];
const INITIAL_DEPOSIT_AMOUNT: u128 = 100;

fn prover_daemon_pool(workers: usize) -> anyhow::Result<ligero_runner::daemon::DaemonPool> {
    static POOL: OnceLock<std::sync::Mutex<Option<ligero_runner::daemon::DaemonPool>>> =
        OnceLock::new();
    let lock = POOL.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock.lock().unwrap();
    if let Some(p) = guard.as_ref() {
        return Ok(p.clone());
    }

    let paths = ligero_runner::LigeroPaths::discover()
        .or_else(|_| Ok::<_, anyhow::Error>(ligero_runner::LigeroPaths::fallback()))?;
    let workers = workers.max(1);
    eprintln!(
        "[prover-daemon] starting webgpu_prover --daemon pool: workers={} prover_bin={} shader_dir={}",
        workers,
        paths.prover_bin.display(),
        paths.shader_dir.display()
    );
    let pool = ligero_runner::daemon::DaemonPool::new_prover(&paths, workers)?;
    *guard = Some(pool.clone());
    Ok(pool)
}

#[derive(Clone, Debug)]
struct ContinuousConfig {
    num_wallets: usize,
    initial_deposit: bool,
    per_tx_delay_ms: u64,
    cycle_delay_ms: u64,
    external_node_url: Option<String>,
    external_verifier_url: Option<String>,
    max_concurrent_proofs: usize,
    detailed_wallet_logs: bool,
    continuous: bool,
    managed_mode: bool,
    /// Authority Full Viewing Key for Level-B compliance.
    /// When set, transfer proofs include viewer attestations and txs include encrypted notes.
    authority_fvk: Option<Hash32>,
    /// Maximum number of transfer cycles to run. None means run indefinitely.
    max_cycles: Option<u64>,
}

impl ContinuousConfig {
    fn from_env() -> Result<Self> {
        // Number of wallets defaults to interactive prompt unless provided via env.
        let num_wallets = std::env::var("CONTINUOUS_NUM_WALLETS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0usize);

        let initial_deposit = std::env::var("INITIAL_DEPOSIT")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);

        let per_tx_delay_ms = std::env::var("PER_TX_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let cycle_delay_ms = std::env::var("CYCLE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        let external_node_url = std::env::var("E2E_ROLLUP_EXTERNAL_NODE_URL")
            .ok()
            .or_else(|| Some("http://localhost:12346".to_string()));
        let external_verifier_url = std::env::var("E2E_ROLLUP_EXTERNAL_VERIFIER_URL")
            .ok()
            .or_else(|| Some("http://localhost:8080".to_string()));

        let max_concurrent_proofs = std::env::var("MAX_CONCURRENT_PROOFS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let detailed_wallet_logs = std::env::var("DETAILED_WALLET_LOGS")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let continuous = std::env::var("CONTINUOUS")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let managed_mode = std::env::var("MANAGED_MODE")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Load authority viewing key for Level-B compliance
        let authority_fvk = load_authority_fvk();

        let max_cycles = std::env::var("MAX_CYCLES")
            .ok()
            .and_then(|v| v.parse().ok());

        Ok(Self {
            num_wallets,
            initial_deposit,
            per_tx_delay_ms,
            cycle_delay_ms,
            external_node_url,
            external_verifier_url,
            max_concurrent_proofs,
            detailed_wallet_logs,
            continuous,
            managed_mode,
            authority_fvk,
            max_cycles,
        })
    }
}

#[derive(Clone)]
struct WalletState {
    account: PrivateKeyAndAddress<DemoRollupSpec>,
    nonce: u64,
    value: u128,
    rho: Hash32,
    spend_sk: Hash32,
    sender_id: Hash32,
}

fn rollup_crate_dir() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("examples/rollup-ligero").exists())
        .ok_or_else(|| anyhow!("Could not find repository root"))?;
    Ok(repo_root.join("examples/rollup-ligero"))
}

fn confirm_and_wipe_demo_data(crate_dir: &Path) -> Result<()> {
    let demo_data = crate_dir.join("demo_data");
    if !demo_data.exists() {
        return Ok(());
    }

    // Check if we should skip the confirmation prompt
    let skip_confirm = std::env::var("MANAGED_MODE_SKIP_CONFIRM")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);

    eprintln!(
        "[managed-mode] This will DELETE all data under {}",
        demo_data.display()
    );

    if !skip_confirm {
        eprint!("Type 'yes' to continue (anything else aborts): ");
        io::stdout().flush().ok();

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read confirmation")?;
        let trimmed = input.trim().to_ascii_lowercase();
        if trimmed != "yes" {
            bail!("Aborted by user; demo_data preserved");
        }
    } else {
        eprintln!(
            "[managed-mode] MANAGED_MODE_SKIP_CONFIRM=1 set, proceeding without confirmation"
        );
    }

    fs::remove_dir_all(&demo_data)
        .with_context(|| format!("Failed to delete {}", demo_data.display()))?;
    fs::create_dir_all(&demo_data)
        .with_context(|| format!("Failed to recreate {}", demo_data.display()))?;

    Ok(())
}

struct ManagedStack {
    api_url: String,
    verifier_url: String,
    chain_hash: [u8; 32],
    _temp_dir: TempDir,
    _child_guard: ChildGuard,
}

fn make_temp_config(base_config: &str, crate_dir: &Path) -> String {
    // Rewrite DA connection to point at the real demo_data in the repo so managed mode works from any CWD.
    let da_conn = format!(
        "connection_string = \"sqlite://{}/demo_data/da.sqlite?mode=rwc\"",
        crate_dir.display()
    );

    let mut out = String::with_capacity(base_config.len() + 64);
    for line in base_config.lines() {
        let l = line.trim_start();
        if l.starts_with("connection_string = ") {
            out.push_str(&da_conn);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

async fn start_managed_stack(
    ligero_env: &LigeroEnv,
    config: &ContinuousConfig,
) -> Result<ManagedStack> {
    let crate_dir = rollup_crate_dir()?;
    let bin_path = find_rollup_binary()?;

    let base_cfg_path = crate_dir.join("rollup_config.toml");
    let base_cfg = std::fs::read_to_string(&base_cfg_path)
        .with_context(|| format!("Failed to read base config at {}", base_cfg_path.display()))?;

    let temp = tempfile::tempdir()?;
    let new_cfg = make_temp_config(&base_cfg, &crate_dir);
    let cfg_path = temp.path().join("rollup_config.toml");
    std::fs::write(&cfg_path, new_cfg)?;

    // Parse bind_host/bind_port and DA connection string from the (verbatim) config.
    let cfg_value: TomlValue = toml::from_str(&base_cfg)
        .with_context(|| "Failed to parse rollup_config.toml for managed mode")?;
    let bind_host = cfg_value
        .get("runner")
        .and_then(|r| r.get("http_config"))
        .and_then(|h| h.get("bind_host"))
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    let bind_port = cfg_value
        .get("runner")
        .and_then(|r| r.get("http_config"))
        .and_then(|h| h.get("bind_port"))
        .and_then(|v| v.as_integer())
        .unwrap_or(12346);
    let da_connection_string = format!(
        "sqlite://{}/demo_data/da.sqlite?mode=rwc",
        crate_dir.display()
    );

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
        .env("LIGERO_PROGRAM_PATH", &ligero_env.program_path)
        .env("LIGERO_PACKING", "8192")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sov-rollup-ligero")?;

    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                eprintln!("[node stdout] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                eprintln!("[node stderr] {}", line);
            }
        });
    }

    let api_host = if bind_host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind_host
    };
    let api_url = format!("http://{}:{}", api_host, bind_port);
    let client = NodeClient::new_unchecked(&api_url);
    wait_for_ready(&client, Duration::from_secs(90)).await?;

    #[derive(Deserialize)]
    struct SchemaRespLocal {
        chain_hash: String,
    }
    let schema: SchemaRespLocal = client
        .query_rest_endpoint("/rollup/schema")
        .await
        .context("Failed to fetch /rollup/schema from managed node")?;
    let chain_hash_hex = schema.chain_hash.trim_start_matches("0x");
    let chain_hash_vec =
        hex::decode(chain_hash_hex).with_context(|| "Invalid chain_hash returned by node")?;
    if chain_hash_vec.len() != 32 {
        bail!("chain_hash must be 32 bytes");
    }
    let mut chain_hash = [0u8; 32];
    chain_hash.copy_from_slice(&chain_hash_vec);

    let verifier_parallelism = std::cmp::max(1, config.max_concurrent_proofs);
    let verifier_url = start_local_verifier(
        &api_url,
        ligero_env.method_id,
        &da_connection_string,
        verifier_parallelism,
        false,
    )
    .await?;

    Ok(ManagedStack {
        api_url,
        verifier_url,
        chain_hash,
        _temp_dir: temp,
        _child_guard: ChildGuard::new(child),
    })
}

#[derive(Deserialize, Clone)]
struct TreeState {
    root: Vec<u8>,
    next_position: u64,
    #[serde(default)]
    #[allow(dead_code)]
    depth: Option<u8>,
}

#[derive(Deserialize, Clone)]
struct NoteInfo {
    position: u64,
    commitment: Vec<u8>,
}

#[derive(Deserialize)]
struct NotesResp {
    notes: Vec<NoteInfo>,
    #[serde(default)]
    current_root: Option<Vec<u8>>,
    #[serde(default)]
    #[allow(dead_code)]
    count: Option<u64>,
}

async fn fetch_note_positions(
    client: &NodeClient,
    detailed_wallet_logs: bool,
) -> Result<HashMap<[u8; 32], u64>> {
    let mut pos_by_cm: HashMap<[u8; 32], u64> = HashMap::new();
    let batch_size = 1000;
    let mut offset = 0;

    loop {
        let endpoint = format!(
            "/modules/midnight-privacy/notes?limit={}&offset={}",
            batch_size, offset
        );
        let batch_resp: NotesResp = client
            .query_rest_endpoint(&endpoint)
            .await
            .with_context(|| format!("Failed to query notes batch at offset {}", offset))?;

        for n in batch_resp.notes.iter() {
            if n.commitment.len() == 32 {
                let mut cm = [0u8; 32];
                cm.copy_from_slice(&n.commitment);
                pos_by_cm.insert(cm, n.position);
            }
        }

        let len = batch_resp.notes.len();
        if len < batch_size {
            break;
        }
        offset += batch_size;
    }

    if detailed_wallet_logs {
        eprintln!(
            "[cycle] refreshed note positions: count={}",
            pos_by_cm.len()
        );
    }

    Ok(pos_by_cm)
}

#[derive(Debug, Deserialize, Clone)]
struct VerifierMetrics {
    deserialize_ms: f64,
    parse_ms: f64,
    signature_verify_ms: f64,
    proof_verify_ms: f64,
    tx_creation_ms: f64,
    node_submit_ms: f64,
    total_ms: f64,
}

#[derive(Deserialize)]
struct VerifierResponse {
    #[allow(dead_code)]
    success: bool,
    tx_hash: Option<String>,
    #[allow(dead_code)]
    sequencer_response: Option<serde_json::Value>,
    #[allow(dead_code)]
    error: Option<String>,
    metrics: VerifierMetrics,
}

#[derive(Deserialize, Clone)]
struct RootsResp {
    recent_roots: Vec<Hash32>,
}

#[derive(Clone, Debug)]
struct CycleSummary {
    num_transfers: usize,
    num_included: usize,
    batches: BTreeMap<u64, usize>,
    avg_worker_ms: f64,
    avg_sequencer_ms: f64,
    avg_worker_proof_ms: f64,
    avg_worker_db_ms: f64,
    avg_seq_decode_ms: f64,
    avg_seq_wrap_ms: f64,
    avg_seq_submit_ms: f64,
    avg_seq_await_ms: f64,
    avg_seq_stf_ms: f64,
}

fn wait_for_c_to_continue(prompt: &str, config: &ContinuousConfig) -> Result<()> {
    use std::io::Write;

    eprintln!("{}", prompt);

    if config.continuous {
        // In continuous mode, just sleep for cycle_delay_ms instead of waiting for user input
        std::thread::sleep(Duration::from_millis(config.cycle_delay_ms));
    } else {
        eprint!("Press Enter to continue...");
        std::io::stdout().flush().ok();

        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .context("Failed to read from stdin")?;
    }

    Ok(())
}

/// Run the continuous transfers loop.
pub async fn run() -> Result<()> {
    let mut config = ContinuousConfig::from_env()?;

    // Prompt the user for the number of wallets if not set.
    if config.num_wallets == 0 {
        use std::io::{self, Write};

        loop {
            eprint!("[config] Enter number of wallets: ");
            io::stdout().flush().ok();

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .context("Failed to read number of wallets from stdin")?;

            let trimmed = input.trim();
            match trimmed.parse::<usize>() {
                Ok(n) if n > 0 => {
                    config.num_wallets = n;
                    break;
                }
                _ => {
                    eprintln!(
                        "Invalid number of wallets '{}', please enter a positive integer.",
                        trimmed
                    );
                }
            }
        }
    }

    eprintln!(
        "[config] wallets={} initial_deposit={} per_tx_delay_ms={} cycle_delay_ms={} max_concurrent_proofs={}",
        config.num_wallets,
        config.initial_deposit,
        config.per_tx_delay_ms,
        config.cycle_delay_ms,
        config.max_concurrent_proofs
    );
    eprintln!(
        "[config] node_url={} verifier_url={} managed_mode={}",
        config
            .external_node_url
            .as_deref()
            .unwrap_or("<managed (local)>"),
        config
            .external_verifier_url
            .as_deref()
            .unwrap_or("<managed (local)>"),
        config.managed_mode
    );
    if let Some(ref fvk) = config.authority_fvk {
        eprintln!(
            "[config] AUTHORITY_FVK set: Level-B viewing attestations ENABLED (fvk={}...)",
            hex::encode(&fvk[..8])
        );
    } else {
        eprintln!("[config] AUTHORITY_FVK not set: transfers will NOT emit authority ciphertexts");
    }

    // Setup Ligero environment (program path, prover/verifier bins, shaders, method id)
    let ligero_env = setup_ligero_env()?;
    let program_path = ligero_env.program_path.clone();

    // Start local stack if endpoints not provided.
    let mut managed_stack: Option<ManagedStack> = None;
    let (node_url, verifier_url) = if config.managed_mode {
        confirm_and_wipe_demo_data(&rollup_crate_dir()?)?;
        let managed = start_managed_stack(&ligero_env, &config).await?;
        let node = managed.api_url.clone();
        let verifier = managed.verifier_url.clone();
        managed_stack = Some(managed);
        (node, verifier)
    } else {
        let node = config.external_node_url.clone().ok_or_else(|| {
            anyhow!("E2E_ROLLUP_EXTERNAL_NODE_URL must be set unless MANAGED_MODE=1")
        })?;
        let verifier = config.external_verifier_url.clone().ok_or_else(|| {
            anyhow!("E2E_ROLLUP_EXTERNAL_VERIFIER_URL must be set unless MANAGED_MODE=1")
        })?;
        (node, verifier)
    };
    eprintln!(
        "[config] using node_url={} verifier_url={}",
        node_url, verifier_url
    );

    let client = NodeClient::new_unchecked(&node_url);
    let http = HttpClient::new();

    // Fetch chain hash for signing
    #[derive(Deserialize)]
    struct SchemaResp {
        chain_hash: String,
    }
    if managed_stack.is_some() {
        wait_for_ready(&client, Duration::from_secs(90)).await?;
    }
    let chain_hash: [u8; 32] = if let Some(ms) = managed_stack.as_ref() {
        ms.chain_hash
    } else {
        let schema: SchemaResp = client
            .query_rest_endpoint("/rollup/schema")
            .await
            .context("Failed to fetch /rollup/schema")?;
        let chain_hash_hex = schema.chain_hash.trim_start_matches("0x");
        let chain_hash_vec =
            hex::decode(chain_hash_hex).with_context(|| "Invalid chain_hash returned by node")?;
        if chain_hash_vec.len() != 32 {
            bail!("chain_hash must be 32 bytes");
        }
        let mut chain_hash_arr = [0u8; 32];
        chain_hash_arr.copy_from_slice(&chain_hash_vec);
        chain_hash_arr
    };

    // Ensure the node is serving /health and the sequencer is ready before sending deposits.
    wait_for_sequencer_ready(&http, &node_url, Duration::from_secs(60)).await?;

    // Load genesis keypairs
    let crate_dir = rollup_crate_dir()?;
    let keypairs_path = crate_dir
        .parent()
        .unwrap()
        .join("test-data/genesis/demo/mock/generated_keypairs.json");

    if !keypairs_path.exists() {
        bail!(
            "Generated keypairs file not found at {}. Run: cargo run -p sov-rollup-ligero --bin generate-genesis-keys",
            keypairs_path.display()
        );
    }

    let keypairs_json = std::fs::read_to_string(&keypairs_path)
        .with_context(|| format!("Failed to read keypairs at {}", keypairs_path.display()))?;
    let all_keypairs: Vec<PrivateKeyAndAddress<DemoRollupSpec>> =
        serde_json::from_str(&keypairs_json)
            .with_context(|| "Failed to parse generated_keypairs.json")?;

    if all_keypairs.len() < config.num_wallets {
        bail!(
            "Not enough keypairs in genesis file. Need {}, but only {} available.",
            config.num_wallets,
            all_keypairs.len()
        );
    }

    let node_base_url = node_url.clone();
    let wallet_setup_start = Instant::now();
    let mut wallets: Vec<WalletState> = Vec::with_capacity(config.num_wallets);
    for i in 0..config.num_wallets {
        let account = all_keypairs[i].clone();
        let nonce = fetch_initial_nonce(&http, &node_base_url, &account)
            .await
            .with_context(|| {
                format!(
                    "Failed to fetch latest nonce/generation for wallet {} (address={})",
                    i, account.address
                )
            })?;

        if config.detailed_wallet_logs {
            eprintln!(
                "[setup] wallet {} address={} starting_nonce={}",
                i, account.address, nonce
            );
        }

        wallets.push(WalletState {
            account,
            nonce,
            value: 0,
            rho: [0u8; 32],
            spend_sk: [0u8; 32],
            sender_id: [0u8; 32],
        });
    }
    let wallet_setup_ms = wallet_setup_start.elapsed().as_secs_f64() * 1000.0;

    // Optional initial deposits to create notes for each wallet
    if config.initial_deposit {
        eprintln!(
            "\n[setup] Performing initial deposits for {} wallets...",
            wallets.len()
        );
        let deposit_start = Instant::now();
        perform_initial_deposits(
            &client,
            &http,
            &verifier_url,
            &mut wallets,
            &chain_hash,
            config.per_tx_delay_ms,
            config.detailed_wallet_logs,
        )
        .await?;
        let deposit_ms = deposit_start.elapsed().as_secs_f64() * 1000.0;

        let total_setup_ms = wallet_setup_ms + deposit_ms;
        eprintln!(
            "[setup] Setup {} wallets in {:.2} ms",
            config.num_wallets, wallet_setup_ms
        );
        eprintln!(
            "[setup] Deposited {} tokens in each wallet in {:.2} ms",
            INITIAL_DEPOSIT_AMOUNT, deposit_ms
        );
        eprintln!(
            "[setup] Total time for initial setup (wallets + deposits): {:.2} ms",
            total_setup_ms
        );
    } else {
        bail!(
            "initial_deposit=false is not yet supported (script needs note secrets to spend). Enable INITIAL_DEPOSIT=1."
        );
    }

    // Main loop: repeated transfer cycles
    if let Some(max) = config.max_cycles {
        eprintln!(
            "\n[loop] Starting transfer cycles (max_cycles={}, Ctrl+C to stop)...",
            max
        );
    } else {
        eprintln!("\n[loop] Starting continuous transfer cycles (Ctrl+C to stop)...");
    }
    let mut cycle_idx: u64 = 0;
    let mut total_transfers: usize = 0;
    let mut total_included: usize = 0;
    let mut total_batches: BTreeMap<u64, usize> = BTreeMap::new();
    let mut total_worker_ms: f64 = 0.0;
    let mut total_worker_proof_ms: f64 = 0.0;
    let mut total_worker_db_ms: f64 = 0.0;
    let mut total_worker_samples: usize = 0;
    let mut total_sequencer_ms: f64 = 0.0;
    let mut total_sequencer_samples: usize = 0;
    let mut total_seq_decode_ms: f64 = 0.0;
    let mut total_seq_wrap_ms: f64 = 0.0;
    let mut total_seq_submit_ms: f64 = 0.0;
    let mut total_seq_await_ms: f64 = 0.0;
    let mut total_seq_stf_ms: f64 = 0.0;
    let mut cached_tree: Option<MerkleTree> = None;
    let mut cached_next_position: u64 = 0;
    let mut cached_root: Option<Hash32> = None;
    let mut cached_pos_by_cm: HashMap<[u8; 32], u64> = HashMap::new();

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        cycle_idx += 1;
        eprintln!("\n[cycle] ===== Transfer cycle {} =====", cycle_idx);

        let summary = tokio::select! {
            _ = &mut shutdown => {
                log_final_summary(
                    cycle_idx - 1,
                    total_transfers,
                    total_included,
                    &total_batches,
                    total_worker_ms,
                    total_worker_proof_ms,
                    total_worker_db_ms,
                    total_worker_samples,
                    total_sequencer_ms,
                    total_sequencer_samples,
                    total_seq_decode_ms,
                    total_seq_wrap_ms,
                    total_seq_submit_ms,
                    total_seq_await_ms,
                    total_seq_stf_ms,
                    config.detailed_wallet_logs,
                );
                return Ok(());
            }
            res = perform_transfer_cycle(
                &client,
                &http,
                &mut wallets,
                &chain_hash,
                &program_path,
                &config,
                &verifier_url,
                &mut cached_tree,
                &mut cached_next_position,
                &mut cached_root,
                &mut cached_pos_by_cm,
            ) => res?,
        };

        total_transfers += summary.num_transfers;
        total_included += summary.num_included;
        for (block_number, count) in summary.batches {
            *total_batches.entry(block_number).or_insert(0) += count;
        }

        if config.detailed_wallet_logs {
            eprintln!(
                "[cycle] Worker breakdown: total_ms={:.2} proof_ms={:.2} db_ms={:.2} sequencer_submit_ms={:.2}",
                summary.avg_worker_ms,
                summary.avg_worker_proof_ms,
                summary.avg_worker_db_ms,
                summary.avg_sequencer_ms
            );
            eprintln!(
                "[cycle] Sequencer breakdown: total_ms={:.2} decode_ms={:.2} wrap_ms={:.2} submit_ms={:.2} await_ms={:.2} stf_ms={:.2}",
                summary.avg_sequencer_ms,
                summary.avg_seq_decode_ms,
                summary.avg_seq_wrap_ms,
                summary.avg_seq_submit_ms,
                summary.avg_seq_await_ms,
                summary.avg_seq_stf_ms,
            );
        }

        // Accumulate global timing metrics, weighted by number of transfers
        total_worker_ms += summary.avg_worker_ms * summary.num_transfers as f64;
        total_worker_proof_ms += summary.avg_worker_proof_ms * summary.num_transfers as f64;
        total_worker_db_ms += summary.avg_worker_db_ms * summary.num_transfers as f64;
        total_worker_samples += summary.num_transfers;
        total_sequencer_ms += summary.avg_sequencer_ms * summary.num_transfers as f64;
        total_sequencer_samples += summary.num_transfers;
        total_seq_decode_ms += summary.avg_seq_decode_ms * summary.num_transfers as f64;
        total_seq_wrap_ms += summary.avg_seq_wrap_ms * summary.num_transfers as f64;
        total_seq_submit_ms += summary.avg_seq_submit_ms * summary.num_transfers as f64;
        total_seq_await_ms += summary.avg_seq_await_ms * summary.num_transfers as f64;
        total_seq_stf_ms += summary.avg_seq_stf_ms * summary.num_transfers as f64;

        eprintln!(
            "[summary] So far: cycles={} transfers={} included={}",
            cycle_idx, total_transfers, total_included
        );

        // Check if we've reached the maximum number of cycles
        if let Some(max) = config.max_cycles {
            if cycle_idx >= max {
                eprintln!("[loop] Reached max_cycles={}, stopping.", max);
                log_final_summary(
                    cycle_idx,
                    total_transfers,
                    total_included,
                    &total_batches,
                    total_worker_ms,
                    total_worker_proof_ms,
                    total_worker_db_ms,
                    total_worker_samples,
                    total_sequencer_ms,
                    total_sequencer_samples,
                    total_seq_decode_ms,
                    total_seq_wrap_ms,
                    total_seq_submit_ms,
                    total_seq_await_ms,
                    total_seq_stf_ms,
                    config.detailed_wallet_logs,
                );
                return Ok(());
            }
        }

        // Interactive gate after each cycle summary.
        wait_for_c_to_continue("[cycle] Cycle summary complete.", &config).ok();

        tokio::select! {
            _ = &mut shutdown => {
                log_final_summary(
                    cycle_idx,
                    total_transfers,
                    total_included,
                    &total_batches,
                    total_worker_ms,
                    total_worker_proof_ms,
                    total_worker_db_ms,
                    total_worker_samples,
                    total_sequencer_ms,
                    total_sequencer_samples,
                    total_seq_decode_ms,
                    total_seq_wrap_ms,
                    total_seq_submit_ms,
                    total_seq_await_ms,
                    total_seq_stf_ms,
                    config.detailed_wallet_logs,
                );
                return Ok(());
            }
            _ = sleep(Duration::from_millis(config.cycle_delay_ms)) => {}
        }
    }
}

fn log_final_summary(
    cycles: u64,
    total_transfers: usize,
    total_included: usize,
    total_batches: &BTreeMap<u64, usize>,
    total_worker_ms: f64,
    total_worker_proof_ms: f64,
    total_worker_db_ms: f64,
    total_worker_samples: usize,
    total_sequencer_ms: f64,
    total_sequencer_samples: usize,
    total_seq_decode_ms: f64,
    total_seq_wrap_ms: f64,
    total_seq_submit_ms: f64,
    total_seq_await_ms: f64,
    total_seq_stf_ms: f64,
    detailed_wallet_logs: bool,
) {
    eprintln!("\n[final-summary] =================================");
    eprintln!(
        "[final-summary] cycles={} total_transfers={} total_included={}",
        cycles, total_transfers, total_included
    );
    let global_worker_avg = if total_worker_samples > 0 {
        total_worker_ms / total_worker_samples as f64
    } else {
        0.0
    };
    let global_sequencer_avg = if total_sequencer_samples > 0 {
        total_sequencer_ms / total_sequencer_samples as f64
    } else {
        0.0
    };
    let global_seq_decode_avg = if total_sequencer_samples > 0 {
        total_seq_decode_ms / total_sequencer_samples as f64
    } else {
        0.0
    };
    let global_seq_wrap_avg = if total_sequencer_samples > 0 {
        total_seq_wrap_ms / total_sequencer_samples as f64
    } else {
        0.0
    };
    let global_seq_submit_avg = if total_sequencer_samples > 0 {
        total_seq_submit_ms / total_sequencer_samples as f64
    } else {
        0.0
    };
    let global_seq_await_avg = if total_sequencer_samples > 0 {
        total_seq_await_ms / total_sequencer_samples as f64
    } else {
        0.0
    };
    let global_seq_stf_avg = if total_sequencer_samples > 0 {
        total_seq_stf_ms / total_sequencer_samples as f64
    } else {
        0.0
    };
    let global_worker_proof_avg = if total_worker_samples > 0 {
        total_worker_proof_ms / total_worker_samples as f64
    } else {
        0.0
    };
    let global_worker_db_avg = if total_worker_samples > 0 {
        total_worker_db_ms / total_worker_samples as f64
    } else {
        0.0
    };
    if detailed_wallet_logs {
        eprintln!(
            "[final-summary] avg_worker_total_ms={:.2} avg_worker_proof_ms={:.2} avg_worker_db_ms={:.2} avg_sequencer_submit_ms={:.2}",
            global_worker_avg,
            global_worker_proof_avg,
            global_worker_db_avg,
            global_sequencer_avg
        );
        eprintln!(
            "[final-summary] sequencer_breakdown_ms: total_ms={:.2} decode_ms={:.2} wrap_ms={:.2} submit_ms={:.2} await_ms={:.2} stf_ms={:.2}",
            global_sequencer_avg,
            global_seq_decode_avg,
            global_seq_wrap_avg,
            global_seq_submit_avg,
            global_seq_await_avg,
            global_seq_stf_avg,
        );
    }
    eprintln!("[final-summary] Block number distribution across all cycles:");
    for (block_number, count) in total_batches {
        eprintln!(
            "[final-summary]   Block number {:3}: {:3} transfers",
            block_number, count
        );
    }
    eprintln!("[final-summary] =================================\n");
}

async fn perform_initial_deposits(
    client: &NodeClient,
    http: &HttpClient,
    verifier_url: &str,
    wallets: &mut [WalletState],
    chain_hash: &[u8; 32],
    per_tx_delay_ms: u64,
    detailed_wallet_logs: bool,
) -> Result<()> {
    for (i, wallet) in wallets.iter_mut().enumerate() {
        let amount: u128 = INITIAL_DEPOSIT_AMOUNT;
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

        let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
            default_test_signed_transaction(
                &wallet.account.private_key,
                &call,
                wallet.nonce,
                chain_hash,
            );
        let mut meter = sov_modules_api::gas::UnlimitedGasMeter::<DemoRollupSpec>::default();
        tx.verify(chain_hash, &mut meter)
            .context("Deposit tx signature verification failed")?;
        let tx_bytes = borsh::to_vec(&tx)?;
        let tx_b64 = BASE64_STANDARD.encode(&tx_bytes);

        let url = format!("{}/midnight-privacy", verifier_url.trim_end_matches('/'));
        if detailed_wallet_logs {
            eprintln!(
                "[deposit] wallet={} nonce={} amount={} via verifier {}",
                i, wallet.nonce, amount, url
            );
        }

        let resp = http
            .post(&url)
            .json(&json!({ "body": tx_b64 }))
            .send()
            .await
            .context("Deposit HTTP request to verifier failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Deposit for wallet {} failed via verifier with status {}: {}",
                i,
                status,
                body
            );
        }

        wallet.nonce += 1;
        wallet.value = amount;
        wallet.rho = rho;
        wallet.spend_sk = spend_sk;
        // Deposit convention: sender_id == recipient.
        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &wallet.spend_sk);
        wallet.sender_id = recipient_from_sk_v2(&DOMAIN, &wallet.spend_sk, &pk_ivk);

        if per_tx_delay_ms > 0 {
            sleep(Duration::from_millis(per_tx_delay_ms)).await;
        }
    }

    // Flush any queued deposits to the sequencer to ensure inclusion before proceeding.
    let flush_endpoint = format!(
        "{}/midnight-privacy/flush",
        verifier_url.trim_end_matches('/')
    );
    let flush_resp = http
        .post(&flush_endpoint)
        .send()
        .await
        .context("Deposit flush request failed")?;
    let status = flush_resp.status();
    let body = flush_resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Deposit flush failed with status {}: {}", status, body);
    } else if detailed_wallet_logs {
        eprintln!("[deposit] flushed queued deposits via {}", flush_endpoint);
    }

    // Wait for all deposit notes to be indexed in the tree before returning.
    // This ensures the first transfer cycle can find the note commitments.
    const DEPOSIT_SYNC_TIMEOUT_SECS: u64 = 60;
    const DEPOSIT_SYNC_POLL_MS: u64 = 100;

    let mut expected_commitments: Vec<[u8; 32]> = Vec::with_capacity(wallets.len());
    for wallet in wallets.iter() {
        let value_u64: u64 = wallet
            .value
            .try_into()
            .context("wallet note value does not fit into u64 (required by note_spend_guest v2)")?;
        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &wallet.spend_sk);
        let recipient = recipient_from_sk_v2(&DOMAIN, &wallet.spend_sk, &pk_ivk);
        let cm = note_commitment(
            &DOMAIN,
            value_u64,
            &wallet.rho,
            &recipient,
            &wallet.sender_id,
        );
        expected_commitments.push(cm);
    }

    if !expected_commitments.is_empty() {
        eprintln!(
            "[deposit] Waiting for {} deposit notes to be indexed...",
            expected_commitments.len()
        );
        let sync_start = Instant::now();
        let sync_deadline = sync_start + Duration::from_secs(DEPOSIT_SYNC_TIMEOUT_SECS);
        let mut pending: HashSet<[u8; 32]> = expected_commitments.iter().copied().collect();

        while !pending.is_empty() && Instant::now() < sync_deadline {
            let fresh_positions = fetch_note_positions(client, false).await?;
            pending.retain(|cm| !fresh_positions.contains_key(cm));

            if !pending.is_empty() {
                sleep(Duration::from_millis(DEPOSIT_SYNC_POLL_MS)).await;
            }
        }

        let sync_elapsed = sync_start.elapsed();
        if pending.is_empty() {
            eprintln!(
                "[deposit] All {} deposit notes indexed in {:.2} ms",
                expected_commitments.len(),
                sync_elapsed.as_secs_f64() * 1000.0
            );
        } else {
            bail!(
                "Timeout: {} of {} deposit notes not indexed after {:.2}s",
                pending.len(),
                expected_commitments.len(),
                sync_elapsed.as_secs_f64()
            );
        }
    }

    Ok(())
}

#[derive(Deserialize)]
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

async fn wait_for_sequencer_ready(
    http: &HttpClient,
    node_url: &str,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    let url = format!("{}/sequencer/ready", node_url.trim_end_matches('/'));
    loop {
        if start.elapsed() > timeout {
            bail!("Timeout waiting for sequencer readiness at {}", url);
        }
        if http
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }
}

async fn perform_transfer_cycle(
    client: &NodeClient,
    http: &HttpClient,
    wallets: &mut [WalletState],
    chain_hash: &[u8; 32],
    program_path: &str,
    config: &ContinuousConfig,
    verifier_url: &str,
    cached_tree: &mut Option<MerkleTree>,
    cached_next_position: &mut u64,
    cached_root: &mut Option<Hash32>,
    cached_pos_by_cm: &mut HashMap<[u8; 32], u64>,
) -> Result<CycleSummary> {
    // Fetch tree state incrementally; reuse cached tree when possible to avoid O(n) rebuilds.
    let tree_rebuild_phase_start = Instant::now();
    let mut attempts_made = 0;
    let mut mt = cached_tree
        .take()
        .unwrap_or_else(|| MerkleTree::new(TREE_DEPTH));
    let mut pos_by_cm = std::mem::take(cached_pos_by_cm);
    let mut cached_root_val = *cached_root;
    let mut cached_next_pos = *cached_next_position;
    let (state, _state_root) = {
        let mut attempt_result = None;
        let mut used_fallback = false;

        for attempt in 0..TREE_REBUILD_MAX_RETRIES {
            attempts_made = attempt + 1;

            // On the last attempt, use bomb-proof full rebuild
            let force_full_rebuild = attempt == TREE_REBUILD_MAX_RETRIES - 1;
            if force_full_rebuild && !used_fallback {
                eprintln!(
                    "[cycle] Incremental rebuild failed, falling back to full rebuild from scratch"
                );
                mt = MerkleTree::new(TREE_DEPTH);
                pos_by_cm.clear();
                cached_next_pos = 0;
                cached_root_val = None;
                used_fallback = true;
            }

            let state_attempt: TreeState = client
                .query_rest_endpoint("/modules/midnight-privacy/tree/state")
                .await
                .context("Failed to query tree state")?;

            anyhow::ensure!(
                state_attempt.root.len() == 32,
                "Tree state root has unexpected length: {}",
                state_attempt.root.len()
            );
            let mut attempt_root = [0u8; 32];
            attempt_root.copy_from_slice(&state_attempt.root);

            let mut reuse_cache = false;
            if let Some(root) = cached_root_val {
                if root == attempt_root && cached_next_pos == state_attempt.next_position {
                    reuse_cache = true;
                } else if state_attempt.next_position < cached_next_pos {
                    // Chain rewound; drop cache and rebuild from scratch.
                    mt = MerkleTree::new(TREE_DEPTH);
                    pos_by_cm.clear();
                    cached_next_pos = 0;
                }
            }

            if !reuse_cache {
                // Grow once to the target size to amortize growth cost.
                let target_leaves = state_attempt.next_position as usize;
                if target_leaves > mt.len() {
                    mt.grow_to_fit(target_leaves);
                }

                // Fetch notes and use current_root from response for atomic consistency.
                // This avoids race conditions between separate /tree/state and /notes calls.
                let batch_size = 1000;
                let mut offset = cached_next_pos as usize;
                let mut last_current_root: Option<Vec<u8>> = None;

                while offset < target_leaves {
                    let endpoint = format!(
                        "/modules/midnight-privacy/notes?limit={}&offset={}",
                        batch_size, offset
                    );
                    let batch_resp: NotesResp = client
                        .query_rest_endpoint(&endpoint)
                        .await
                        .with_context(|| {
                            format!("Failed to query notes batch at offset {}", offset)
                        })?;

                    if batch_resp.notes.is_empty() {
                        break;
                    }

                    // Capture current_root from the response for atomic consistency
                    if let Some(ref root) = batch_resp.current_root {
                        last_current_root = Some(root.clone());
                    }

                    for n in batch_resp.notes.iter() {
                        if n.commitment.len() == 32 {
                            let mut cm = [0u8; 32];
                            cm.copy_from_slice(&n.commitment);
                            if n.position as usize >= mt.len() {
                                mt.grow_to_fit(n.position as usize + 1);
                            }
                            mt.set_leaf(n.position as usize, cm);
                            pos_by_cm.insert(cm, n.position);
                        }
                    }

                    let len = batch_resp.notes.len();
                    offset += len;
                }

                // Prefer using current_root from notes response (atomic with notes data)
                // Fall back to state_attempt.root if not available
                if let Some(ref api_root) = last_current_root {
                    if api_root.len() == 32 {
                        attempt_root.copy_from_slice(api_root);
                    }
                }
            }

            if mt.root().as_slice() == attempt_root.as_slice() {
                // Update state_attempt.root to match attempt_root for consistency
                let mut final_state = state_attempt.clone();
                final_state.root = attempt_root.to_vec();
                cached_root_val = Some(attempt_root);
                cached_next_pos = final_state.next_position;
                attempt_result = Some((final_state, attempt_root));
                break;
            } else {
                // Roots don't match - reset cache for next attempt
                eprintln!(
                    "[cycle] Tree root mismatch on attempt {}: rebuilt={} vs expected={}",
                    attempt + 1,
                    hex::encode(mt.root()),
                    hex::encode(&attempt_root)
                );
                mt = MerkleTree::new(TREE_DEPTH);
                pos_by_cm.clear();
                cached_next_pos = 0;
                cached_root_val = None;
                sleep(Duration::from_millis(TREE_REBUILD_RETRY_DELAY_MS)).await;
                continue;
            }
        }

        attempt_result.expect("Tree rebuild attempt must succeed or bail")
    };
    let tree_rebuild_phase_elapsed = tree_rebuild_phase_start.elapsed();
    eprintln!(
        "[cycle] tree rebuild finished in {:.2} ms after {} attempt(s)",
        tree_rebuild_phase_elapsed.as_secs_f64() * 1000.0,
        attempts_made
    );

    let roots_state: RootsResp = client
        .query_rest_endpoint("/modules/midnight-privacy/roots/recent")
        .await
        .context("Failed to query recent roots")?;

    let mut anchor_root = [0u8; 32];
    if let Some(last_root) = roots_state.recent_roots.last() {
        anchor_root = *last_root;
    } else {
        anchor_root.copy_from_slice(&state.root);
    }

    #[derive(Clone)]
    struct TransferInput {
        wallet_idx: usize,
        value: u128,
        rho: Hash32,
        spend_sk: Hash32,
        sender_id: Hash32,
        position: u64,
    }

    let mut inputs: Vec<TransferInput> = Vec::new();
    for (idx, wallet) in wallets.iter().enumerate() {
        if wallet.value == 0 {
            continue;
        }
        let value_u64: u64 = wallet
            .value
            .try_into()
            .context("wallet note value does not fit into u64 (required by note_spend_guest v2)")?;
        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &wallet.spend_sk);
        let recipient = recipient_from_sk_v2(&DOMAIN, &wallet.spend_sk, &pk_ivk);
        let cm = note_commitment(
            &DOMAIN,
            value_u64,
            &wallet.rho,
            &recipient,
            &wallet.sender_id,
        );
        let mut position = pos_by_cm.get(&cm).copied();

        if position.is_none() {
            // Retry with fresh note fetches and small waits; useful when the tree has just advanced.
            for _attempt in 0..MISSING_NOTE_RETRY_MAX {
                sleep(Duration::from_millis(MISSING_NOTE_RETRY_DELAY_MS)).await;
                pos_by_cm = fetch_note_positions(client, config.detailed_wallet_logs).await?;
                position = pos_by_cm.get(&cm).copied();
                if position.is_some() {
                    break;
                }
            }
        }

        if let Some(position) = position {
            inputs.push(TransferInput {
                wallet_idx: idx,
                value: wallet.value,
                rho: wallet.rho,
                spend_sk: wallet.spend_sk,
                sender_id: wallet.sender_id,
                position,
            });
        } else {
            eprintln!(
                "[cycle] wallet {}: note commitment not yet in tree; skipping this cycle",
                idx
            );
        }
    }

    if inputs.is_empty() {
        *cached_tree = Some(mt);
        *cached_next_position = cached_next_pos;
        *cached_root = cached_root_val;
        *cached_pos_by_cm = pos_by_cm;
        eprintln!("[cycle] No spendable notes found in tree; nothing to do.");
        return Ok(CycleSummary {
            num_transfers: 0,
            num_included: 0,
            batches: BTreeMap::new(),
            avg_worker_ms: 0.0,
            avg_sequencer_ms: 0.0,
            avg_worker_proof_ms: 0.0,
            avg_worker_db_ms: 0.0,
            avg_seq_decode_ms: 0.0,
            avg_seq_wrap_ms: 0.0,
            avg_seq_submit_ms: 0.0,
            avg_seq_await_ms: 0.0,
            avg_seq_stf_ms: 0.0,
        });
    }

    eprintln!(
        "[cycle] Building proofs for {} wallets (max_concurrent_proofs={})",
        inputs.len(),
        config.max_concurrent_proofs
    );
    let proof_generation_start = Instant::now();

    use sov_rollup_interface::zk::{Zkvm, ZkvmHost};

    let depth_usize = TREE_DEPTH as usize;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_proofs));
    let mut proof_tasks = Vec::with_capacity(inputs.len());

    let authority_fvk = config.authority_fvk;
    for input in inputs.iter() {
        let account_idx = input.wallet_idx;
        let value = input.value;
        let in_rho = input.rho;
        let in_spend_sk = input.spend_sk;
        let in_sender_id = input.sender_id;
        let position = input.position;
        let siblings = mt.open(position as usize);
        let anchor = anchor_root;
        let program_path = program_path.to_string();
        let sem = semaphore.clone();
        let authority_fvk = authority_fvk; // Copy for closure
        let daemon_workers = config.max_concurrent_proofs;
        proof_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, Vec<u8>, Hash32, Hash32)> {
                let value_u64: u64 = value
                    .try_into()
                    .context("note value does not fit into u64 (required by note_spend_guest v2)")?;
                if value_u64 > i64::MAX as u64 {
                    bail!("note value does not fit into i64 (required by note_spend_guest v2 ABI)");
                }

                // note_spend_guest v2 derives the owner recipient from (spend_sk, pk_ivk_owner).
                let pk_ivk_owner = pk_ivk_from_sk(&DOMAIN, &in_spend_sk);
                let in_recipient = recipient_from_sk_v2(&DOMAIN, &in_spend_sk, &pk_ivk_owner);
                let sender_id_out = in_recipient;

                // New output note (same value, fresh rho + fresh address (pk/spend_sk))
                let out_rho: Hash32 = rand::thread_rng().gen();
                let out_spend_sk: Hash32 = rand::thread_rng().gen();
                let out_pk_spend = pk_from_sk(&out_spend_sk);
                let out_pk_ivk = pk_ivk_from_sk(&DOMAIN, &out_spend_sk);
                let out_recipient = recipient_from_pk_v2(&DOMAIN, &out_pk_spend, &out_pk_ivk);
                let cm_out = note_commitment(&DOMAIN, value_u64, &out_rho, &out_recipient, &sender_id_out);

                // Nullifier is derived from spend_sk (nf_key is derived inside the circuit).
                let nf_key = nf_key_from_sk(&DOMAIN, &in_spend_sk);
                let nf = nullifier(&DOMAIN, &nf_key, &in_rho);

                // Build viewer attestation if authority FVK is set
                let (view_attestations, viewer_data) = if let Some(fvk) = authority_fvk {
                    let (att, _enc) = make_viewer_bundle(
                        &fvk, &DOMAIN, value, &out_rho, &out_recipient, &sender_id_out, &cm_out,
                    )?;
                    (Some(vec![att.clone()]), Some((fvk, att)))
                } else {
                    (None, None)
                };

                let public = SpendPublic {
                    anchor_root: anchor,
                    nullifier: nf,
                    withdraw_amount: 0,
                    output_commitments: vec![cm_out],
                    view_attestations,
                };

                let n_out: usize = 1;
                // LigeroConfig private indices are 1-based (by argument position).
                // v2 ABI (no viewers): 1+6 + 1*(4 + 2*depth) + 3 + 5*n_out + 1(inv_enforce)
                let mut private_indices: Vec<usize> = Vec::new();
                private_indices.extend_from_slice(&[2, 3]); // spend_sk, pk_ivk_owner
                private_indices.extend_from_slice(&[7, 8, 9]); // value_in, rho_in, sender_id_in
                // pos_bits [10..10+depth)
                for j in 0..depth_usize {
                    private_indices.push(10 + j);
                }
                // siblings [10+depth..10+2*depth)
                for j in 0..depth_usize {
                    private_indices.push(10 + depth_usize + j);
                }
                // output 0 private args:
                // value_out, rho_out, pk_spend_out, pk_ivk_out
                let out_base = 14 + 2 * depth_usize;
                private_indices.extend_from_slice(&[
                    out_base,         // value_out
                    out_base + 1,     // rho_out
                    out_base + 2,     // pk_spend_out
                    out_base + 3,     // pk_ivk_out
                ]);
                // inv_enforce (private)
                private_indices.push(19 + 2 * depth_usize);
                // Viewer section: fvk is private
                if viewer_data.is_some() {
                    private_indices.push(22 + 2 * depth_usize);
                }

                let mut host =
                    <sov_ligero_adapter::Ligero as Zkvm>::Host::from_args(&program_path)
                        .with_private_indices(private_indices);

                // Typed binary ABI for zkVM performance (matches note_spend_guest argument layout)
                host.add_hex_arg(hex::encode(DOMAIN)); // 1: domain (PUBLIC)
                host.add_hex_arg(hex::encode(in_spend_sk)); // 2: spend_sk (PRIVATE)
                host.add_hex_arg(hex::encode(pk_ivk_owner)); // 3: pk_ivk_owner (PRIVATE)
                host.add_u64_arg(TREE_DEPTH as u64); // 4: depth (PUBLIC)
                host.add_hex_arg(hex::encode(anchor)); // 5: anchor (PUBLIC)
                host.add_u64_arg(1); // 6: n_in (PUBLIC)

                host.add_u64_arg(value_u64); // 7: value_in (PRIVATE)
                host.add_hex_arg(hex::encode(in_rho)); // 8: rho_in (PRIVATE)
                host.add_hex_arg(hex::encode(in_sender_id)); // 9: sender_id_in (PRIVATE)

                // 10..10+depth: position bits (field elements 0/1 as 32-byte BE)
                for lvl in 0..depth_usize {
                    let bit = ((position >> lvl) & 1) as u8;
                    let mut bit_bytes = [0u8; 32];
                    bit_bytes[31] = bit;
                    host.add_hex_arg(hex::encode(bit_bytes));
                }

                // 10+depth..10+2*depth: siblings
                for s in &siblings {
                    host.add_hex_arg(hex::encode(s));
                }

                host.add_hex_arg(hex::encode(nf)); // nullifier (PUBLIC)
                host.add_u64_arg(0); // withdraw_amount (PUBLIC)
                host.add_hex_arg(hex::encode([0u8; 32])); // withdraw_to (PUBLIC; must be 0 for transfers)
                host.add_u64_arg(n_out as u64); // n_out (PUBLIC)

                // output 0 (private except commitment)
                host.add_u64_arg(value_u64);
                host.add_hex_arg(hex::encode(out_rho));
                host.add_hex_arg(hex::encode(out_pk_spend));
                host.add_hex_arg(hex::encode(out_pk_ivk));
                host.add_hex_arg(hex::encode(cm_out));
                // inv_enforce (PRIVATE)
                let inv_enforce = {
                    let mut enforce_prod = Bn254Fr::from_u32(1);
                    enforce_prod.mulmod_checked(&Bn254Fr::from_u64(value_u64));
                    enforce_prod.mulmod_checked(&Bn254Fr::from_u64(value_u64));
                    let mut delta = Bn254Fr::new();
                    let mut out_fr = Bn254Fr::new();
                    out_fr.set_bytes_big(&out_rho);
                    let mut in_fr = Bn254Fr::new();
                    in_fr.set_bytes_big(&in_rho);
                    submod_checked(&mut delta, &out_fr, &in_fr);
                    enforce_prod.mulmod_checked(&delta);
                    let mut inv = enforce_prod.clone();
                    inv.inverse();
                    inv.to_bytes_be()
                };
                host.add_hex_arg(hex::encode(inv_enforce));

                // Viewer section (Level-B)
                if let Some((fvk, att)) = viewer_data {
                    host.add_u64_arg(1); // m_viewers
                    host.add_hex_arg(hex::encode(att.fvk_commitment));
                    host.add_hex_arg(hex::encode(fvk));
                    host.add_hex_arg(hex::encode(att.ct_hash));
                    host.add_hex_arg(hex::encode(att.mac));
                }

                host.set_public_output(&public)
                    .context("set public output (round 2)")?;

                // Daemon-mode prover ONLY: keep webgpu_prover warm and avoid respawning for each proof.
                let proof_data = (|| -> anyhow::Result<Vec<u8>> {
                    let public_output = host.require_public_output()?;
                    let cfg = host.runner().config().clone();
                    let mut cfg_json = serde_json::to_value(&cfg)?;

                    // Daemon-mode prover expects `program` to be a real `.wasm` path, not a circuit name.
                    // `LigeroHost`/`LigeroRunner` can accept circuit names, so resolve here before sending.
                    if let serde_json::Value::Object(ref mut map) = cfg_json {
                        if let Some(serde_json::Value::String(program)) =
                            map.get("program").cloned()
                        {
                            let resolved = ligero_runner::resolve_program(&program)
                                .with_context(|| format!("Failed to resolve program '{program}'"))?;
                            map.insert(
                                "program".to_string(),
                                serde_json::Value::String(resolved.to_string_lossy().to_string()),
                            );
                        }
                    }

                    // Provide an explicit, unique proof output path to the daemon.
                    // Relying on the daemon's internal temp-path generator can collide across
                    // multiple daemon processes started at the same time (same timestamp + per-process counter).
                    let tmp = tempfile::tempdir()?;
                    let proof_path = tmp.path().join("proof_data.bin");
                    if let serde_json::Value::Object(ref mut map) = cfg_json {
                        map.insert(
                            "proof-path".to_string(),
                            serde_json::Value::String(proof_path.to_string_lossy().to_string()),
                        );
                        // Request uncompressed proofs: this significantly reduces CPU overhead
                        // (gzip compress/decompress) while keeping proving/verifying correctness.
                        map.insert("gzip-proof".to_string(), serde_json::Value::Bool(false));
                    }

                    let pool = prover_daemon_pool(daemon_workers)
                        .context("initialize ligero prover daemon pool")?;
                    let resp = pool.prove(cfg_json).context("daemon prove request failed")?;
                    if !resp.ok {
                        anyhow::bail!(
                            "prover daemon returned ok=false (exit_code={:?}): {}",
                            resp.exit_code,
                            resp.error.unwrap_or_else(|| "unknown error".to_string())
                        );
                    }
                    // Daemon will echo `proof_path`, but we read from our explicitly-provided path.
                    let proof_bytes = std::fs::read(&proof_path)
                        .with_context(|| format!("failed to read proof at {}", proof_path.display()))?;
                    drop(tmp); // cleanup temp directory

                    let args_json = serde_json::to_vec(&cfg.args)?;
                    let pkg = ligero_runner::LigeroProofPackage::new(
                        proof_bytes,
                        public_output,
                        args_json,
                        cfg.private_indices.clone(),
                    )?;
                    Ok(bincode::serialize(&pkg)?)
                })()
                .context("generate second-round transfer proof via daemon")?;
                Ok((account_idx, proof_data, out_rho, out_spend_sk))
            })
            .await
            .expect("spawn_blocking join failed")
        }));
    }

    let mut proofs: Vec<(usize, Vec<u8>, Hash32, Hash32)> = Vec::with_capacity(inputs.len());
    for t in proof_tasks {
        proofs.push(t.await??);
    }

    let proof_generation_ms = proof_generation_start.elapsed().as_secs_f64() * 1000.0;
    let avg_proof_ms = if !proofs.is_empty() {
        proof_generation_ms / proofs.len() as f64
    } else {
        0.0
    };
    eprintln!(
        "[cycle] Proof generation: generated {} transfer proofs (one per wallet) in {:.2} ms",
        proofs.len(),
        proof_generation_ms
    );
    eprintln!(
        "[cycle] Proof generation: average {:.2} ms per proof",
        avg_proof_ms
    );

    // Build and send transfer transactions to verifier (deferred submission)
    let transfer_txs_start = Instant::now();
    let mut transfer_txs_b64: Vec<(usize, String)> = Vec::with_capacity(proofs.len());
    let mut transfer_hashes: Vec<String> = Vec::with_capacity(proofs.len());
    eprintln!(
        "[cycle] Building and signing {} transfers txs...",
        proofs.len()
    );

    #[derive(Debug)]
    struct BuiltTransfer {
        idx: usize,
        wallet_idx: usize,
        tx_hash: String,
        tx_b64: String,
        new_nonce: u64,
        new_rho: Hash32,
        new_spend_sk: Hash32,
        new_sender_id: Hash32,
    }

    let mut build_tasks = Vec::with_capacity(proofs.len());
    for (i, (wallet_idx, proof_bytes, out_rho, out_spend_sk)) in proofs.into_iter().enumerate() {
        let wallet = wallets[wallet_idx].clone();
        let chain_hash = *chain_hash;
        let anchor_root = anchor_root;
        let detailed_logs = config.detailed_wallet_logs;
        let authority_fvk = authority_fvk; // Copy for closure
        let value = wallet.value; // The output value (same as input for pure transfer)
        build_tasks.push(tokio::task::spawn_blocking(
            move || -> anyhow::Result<BuiltTransfer> {
                let nf_key = nf_key_from_sk(&DOMAIN, &wallet.spend_sk);
                let nf = nullifier(&DOMAIN, &nf_key, &wallet.rho);

                let value_u64: u64 = value.try_into().context(
                    "note value does not fit into u64 (required by note_spend_guest v2)",
                )?;
                let out_pk_spend = pk_from_sk(&out_spend_sk);
                let out_pk_ivk = pk_ivk_from_sk(&DOMAIN, &out_spend_sk);
                let out_recipient = recipient_from_pk_v2(&DOMAIN, &out_pk_spend, &out_pk_ivk);
                let pk_ivk_owner = pk_ivk_from_sk(&DOMAIN, &wallet.spend_sk);
                let sender_id = recipient_from_sk_v2(&DOMAIN, &wallet.spend_sk, &pk_ivk_owner);

                // Build encrypted note for authority if configured
                let view_ciphertexts: Option<Vec<EncryptedNote>> = match authority_fvk {
                    Some(fvk) => {
                        let cm_out = note_commitment(
                            &DOMAIN,
                            value_u64,
                            &out_rho,
                            &out_recipient,
                            &sender_id,
                        );
                        let (_att, enc) = make_viewer_bundle(
                            &fvk,
                            &DOMAIN,
                            value,
                            &out_rho,
                            &out_recipient,
                            &sender_id,
                            &cm_out,
                        )?;
                        Some(vec![enc])
                    }
                    None => None,
                };

                let call =
                    RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Transfer {
                        proof: proof_bytes
                            .try_into()
                            .map_err(|_| anyhow!("Proof too large for SafeVec"))?,
                        anchor_root,
                        nullifier: nf,
                        view_ciphertexts,
                        gas: None,
                    });

                let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
                    default_test_signed_transaction(
                        &wallet.account.private_key,
                        &call,
                        wallet.nonce,
                        &chain_hash,
                    );

                let mut meter =
                    sov_modules_api::gas::UnlimitedGasMeter::<DemoRollupSpec>::default();
                tx.verify(&chain_hash, &mut meter)
                    .context("Transfer tx signature verify failed")?;

                let tx_bytes = borsh::to_vec(&tx)?;
                let tx_hash = tx.hash().to_string();
                let tx_b64 = BASE64_STANDARD.encode(&tx_bytes);

                if detailed_logs {
                    eprintln!(
                        "  [transfer] wallet={} idx_in_cycle={} nonce={} tx={} nullifier={}",
                        wallet_idx,
                        i + 1,
                        wallet.nonce,
                        tx_hash,
                        hex::encode(&nf[..8])
                    );
                }

                Ok(BuiltTransfer {
                    idx: i,
                    wallet_idx,
                    tx_hash,
                    tx_b64,
                    new_nonce: wallet.nonce + 1,
                    new_rho: out_rho,
                    new_spend_sk: out_spend_sk,
                    new_sender_id: sender_id,
                })
            },
        ));
    }

    let mut built = Vec::with_capacity(build_tasks.len());
    for t in build_tasks {
        built.push(t.await??);
    }
    built.sort_by_key(|b| b.idx);

    for b in built {
        transfer_hashes.push(b.tx_hash);
        transfer_txs_b64.push((b.wallet_idx, b.tx_b64));
        let w = &mut wallets[b.wallet_idx];
        w.nonce = b.new_nonce;
        w.rho = b.new_rho;
        w.spend_sk = b.new_spend_sk;
        w.sender_id = b.new_sender_id;
    }
    let transfer_txs_ms = transfer_txs_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "[cycle] Built and signed {} transfers txs in {:.2} ms (avg {:.2} ms per tx)",
        transfer_txs_b64.len(),
        transfer_txs_ms,
        transfer_txs_ms / transfer_txs_b64.len() as f64
    );

    eprintln!(
        "[cycle] Submitting {} transfers to verifier...",
        transfer_txs_b64.len()
    );

    // Track per-tx worker processing metrics (from verifier)
    let transfer_submit_start = Instant::now();
    let mut worker_metrics_by_hash: HashMap<String, VerifierMetrics> = HashMap::new();
    let submit_endpoint = format!("{}/midnight-privacy", verifier_url);
    let concurrency_limit = config.max_concurrent_proofs.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency_limit));
    let mut join_set = JoinSet::new();

    #[derive(Debug)]
    struct TransferSubmitResult {
        idx: usize,
        wallet_idx: usize,
        worker_hash: String,
        metrics: VerifierMetrics,
    }

    for (idx, (wallet_idx, body_b64)) in transfer_txs_b64.into_iter().enumerate() {
        let client = http.clone();
        let endpoint = submit_endpoint.clone();
        let permit_pool = semaphore.clone();
        let transfer_hash = transfer_hashes[idx].clone();
        let per_tx_delay = config.per_tx_delay_ms;
        const MAX_SUBMIT_RETRIES: usize = 3;
        join_set.spawn(async move {
            let _permit = permit_pool
                .acquire_owned()
                .await
                .expect("submit concurrency semaphore closed");
            if per_tx_delay > 0 {
                sleep(Duration::from_millis(per_tx_delay)).await;
            }
            let display_idx = idx + 1;

            let mut last_err: Option<anyhow::Error> = None;
            for attempt in 0..=MAX_SUBMIT_RETRIES {
                let resp = client
                    .post(&endpoint)
                    .json(&json!({ "body": body_b64 }))
                    .send()
                    .await;

                let resp = match resp {
                    Ok(r) => r,
                    Err(e) => {
                        last_err = Some(e.into());
                        if attempt < MAX_SUBMIT_RETRIES {
                            let backoff_ms = 100 * 2_u64.saturating_pow(attempt as u32);
                            sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        } else {
                            break;
                        }
                    }
                };

                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    last_err = Some(anyhow::anyhow!(
                        "transfer #{} verifier returned status {}: {}",
                        display_idx,
                        status,
                        body
                    ));
                    if attempt < MAX_SUBMIT_RETRIES {
                        let backoff_ms = 100 * 2_u64.saturating_pow(attempt as u32);
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    } else {
                        break;
                    }
                }

                let vresp: VerifierResponse = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = Some(e.into());
                        if attempt < MAX_SUBMIT_RETRIES {
                            let backoff_ms = 100 * 2_u64.saturating_pow(attempt as u32);
                            sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        } else {
                            break;
                        }
                    }
                };

                let worker_hash = vresp.tx_hash.unwrap_or(transfer_hash);
                return Ok::<TransferSubmitResult, anyhow::Error>(TransferSubmitResult {
                    idx,
                    wallet_idx,
                    worker_hash,
                    metrics: vresp.metrics.clone(),
                });
            }

            Err(last_err.unwrap_or_else(|| {
                anyhow::anyhow!(format!(
                    "transfer #{} request failed after retries",
                    display_idx
                ))
            }))
        });
    }

    while let Some(res) = join_set.join_next().await {
        let TransferSubmitResult {
            idx,
            wallet_idx,
            worker_hash,
            metrics,
        } = res??;
        worker_metrics_by_hash.insert(worker_hash.clone(), metrics.clone());

        if config.detailed_wallet_logs {
            let display_idx = idx + 1;
            eprintln!(
                "    [timing][worker] wallet={} idx_in_cycle={} deserialize={:.2}ms parse={:.2}ms sig={:.2}ms proof={:.2}ms db={:.2}ms submit={:.2}ms total={:.2}ms",
                wallet_idx,
                display_idx,
                metrics.deserialize_ms,
                metrics.parse_ms,
                metrics.signature_verify_ms,
                metrics.proof_verify_ms,
                metrics.tx_creation_ms,
                metrics.node_submit_ms,
                metrics.total_ms
            );
        }
    }

    let transfer_submit_ms = transfer_submit_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "[cycle] Submitted {} transfers to verifier in {:.2} ms (avg {:.2} ms per tx)",
        transfer_hashes.len(),
        transfer_submit_ms,
        transfer_submit_ms / transfer_hashes.len() as f64
    );
    // Interactive gate before flushing to the sequencer.
    wait_for_c_to_continue("[cycle] Ready to submit to sequencer.", config).ok();
    let flush_start = Instant::now();
    let resp = http
        .post(format!("{}/midnight-privacy/flush", verifier_url))
        .send()
        .await
        .context("Submit to sequencer request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "Submit to sequencer endpoint returned status {}: {}",
            status,
            body
        );
    }
    let flush_elapsed_ms = flush_start.elapsed().as_secs_f64() * 1000.0;

    #[derive(Deserialize, Clone)]
    struct SeqBreakdown {
        #[serde(default)]
        decode_ms: Option<f64>,
        #[serde(default)]
        wrap_ms: Option<f64>,
        #[serde(default)]
        submit_ms: Option<f64>,
        #[serde(default)]
        await_ms: Option<f64>,
        #[serde(default)]
        total_ms: Option<f64>,
        #[serde(default)]
        stf_execution_ms: Option<f64>,
    }

    #[derive(Deserialize)]
    struct FlushResultEntry {
        tx_hash: Option<String>,
        #[allow(dead_code)]
        accepted: bool,
        #[allow(dead_code)]
        status: Option<u16>,
        #[allow(dead_code)]
        response: Option<serde_json::Value>,
        #[allow(dead_code)]
        error: Option<String>,
        sequencer_ms: Option<f64>,
        sequencer_breakdown: Option<SeqBreakdown>,
    }

    #[derive(Deserialize)]
    struct FlushSummary {
        flushed: usize,
        accepted: usize,
        rejected: usize,
        results: Vec<FlushResultEntry>,
    }

    let flush: FlushSummary =
        serde_json::from_str(&body).context("Failed to parse submit to sequencer JSON response")?;
    if config.detailed_wallet_logs {
        eprintln!(
            "[cycle] Submit to sequencer complete. flushed={} accepted={} rejected={} latency_ms={:.2}",
            flush.flushed, flush.accepted, flush.rejected, flush_elapsed_ms
        );
    } else {
        eprintln!(
            "[cycle] Submit to sequencer complete in {:.2} ms (avg {:.2} ms, {:.2} tps)",
            flush_elapsed_ms,
            flush_elapsed_ms / flush.flushed as f64,
            flush.flushed as f64 / (flush_elapsed_ms / 1000.0)
        );
    }

    // Track per-tx sequencer times and breakdown for this cycle
    let mut sequencer_times_ms: HashMap<String, f64> = HashMap::new();
    let mut sequencer_metrics_by_hash: HashMap<String, SeqBreakdown> = HashMap::new();
    for entry in flush.results {
        if let Some(hash) = entry.tx_hash {
            if let Some(b) = entry.sequencer_breakdown {
                let total_ms = b.total_ms.unwrap_or(0.0);
                let decode_ms = b.decode_ms.unwrap_or(0.0);
                let wrap_ms = b.wrap_ms.unwrap_or(0.0);
                let submit_ms = b.submit_ms.unwrap_or(0.0);
                let await_ms = b.await_ms.unwrap_or(0.0);
                let stf_str = b
                    .stf_execution_ms
                    .map(|v| format!("{:.2}", v))
                    .unwrap_or_else(|| "n/a".to_string());
                if config.detailed_wallet_logs {
                    eprintln!(
                        "    [timing][sequencer] tx={} total={:.2} decode={:.2} wrap={:.2} submit={:.2} await={:.2} stf={}",
                        hash,
                        total_ms,
                        decode_ms,
                        wrap_ms,
                        submit_ms,
                        await_ms,
                        stf_str,
                    );
                }
                sequencer_times_ms.insert(hash.clone(), total_ms);
                sequencer_metrics_by_hash.insert(hash, b);
            } else if let Some(ms) = entry.sequencer_ms {
                if config.detailed_wallet_logs {
                    eprintln!("    [timing][sequencer] tx={} total_ms={:.2}", hash, ms);
                }
                sequencer_times_ms.insert(hash, ms);
            }
        }
    }

    // After flush, verify inclusion and collect per-batch statistics and timing
    let mut batches: BTreeMap<u64, usize> = BTreeMap::new();
    let mut num_included = 0usize;
    let num_transfers = transfer_hashes.len();
    let mut first_included_at: Option<Instant> = None;
    let mut last_included_at: Option<Instant> = None;
    let mut first_included_wall: Option<SystemTime> = None;
    let mut last_included_wall: Option<SystemTime> = None;
    let mut first_batch_number: Option<u64> = None;
    let mut last_batch_number: Option<u64> = None;

    // Aggregate worker / sequencer timing for this cycle
    let mut worker_sum_ms = 0.0f64;
    let mut worker_count = 0usize;
    let mut worker_proof_sum_ms = 0.0f64;
    let mut worker_db_sum_ms = 0.0f64;
    let mut sequencer_sum_ms = 0.0f64;
    let mut sequencer_count = 0usize;
    let mut seq_decode_sum_ms = 0.0f64;
    let mut seq_wrap_sum_ms = 0.0f64;
    let mut seq_submit_sum_ms = 0.0f64;
    let mut seq_await_sum_ms = 0.0f64;
    let mut seq_stf_sum_ms = 0.0f64;

    for hash_hex in &transfer_hashes {
        let deadline = Instant::now() + Duration::from_secs(60);
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
                    let now_instant = Instant::now();
                    let now_wall = SystemTime::now();
                    let batch_number = ltx.batch_number;
                    if first_included_at.is_none() {
                        first_included_at = Some(now_instant);
                        first_included_wall = Some(now_wall);
                        first_batch_number = Some(batch_number);
                    }
                    last_included_at = Some(now_instant);
                    last_included_wall = Some(now_wall);
                    last_batch_number = Some(batch_number);
                    *batches.entry(batch_number).or_insert(0) += 1;
                    num_included += 1;
                    break;
                }
                Err(_) => {
                    if Instant::now() > deadline {
                        eprintln!(
                            "[cycle] Timeout waiting for transfer {} to appear in ledger",
                            hash_hex
                        );
                        break;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    // Summarize when the first and last txs were observed in the ledger.
    if let (
        Some(first_instant),
        Some(last_instant),
        Some(first_wall),
        Some(last_wall),
        Some(first_block),
        Some(last_block),
    ) = (
        first_included_at,
        last_included_at,
        first_included_wall,
        last_included_wall,
        first_batch_number,
        last_batch_number,
    ) {
        let span_ms = last_instant.duration_since(first_instant).as_secs_f64() * 1000.0;

        fn format_time_hhmmss_millis(ts: SystemTime) -> String {
            match ts.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(dur) => DateTime::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos())
                    .map(|dt| dt.with_timezone(&Local))
                    .unwrap_or_else(Local::now)
                    .format("%Y-%m-%d %H:%M:%S%.3f")
                    .to_string(),
                Err(_) => "invalid-system-time".to_string(),
            }
        }

        let first_ts = format_time_hhmmss_millis(first_wall);
        let last_ts = format_time_hhmmss_millis(last_wall);
        eprintln!(
            "[cycle] First tx included in block {} at {}",
            first_block, first_ts
        );
        eprintln!(
            "[cycle] Last tx included in block {} at {}",
            last_block, last_ts
        );
        eprintln!(
            "[cycle] Total span: {:.2} ms, {} blocks, {} total txs",
            span_ms,
            batches.len(),
            num_included
        );

        // Per-block statistics
        for (block_num, tx_count) in &batches {
            eprintln!(
                "[cycle] Block {} generated with {} txs.",
                block_num, tx_count
            );
        }
    }

    // Aggregate timing only for transfers we attempted this cycle
    for hash in &transfer_hashes {
        if let Some(m) = worker_metrics_by_hash.get(hash) {
            worker_sum_ms += m.total_ms;
            worker_proof_sum_ms += m.proof_verify_ms;
            worker_db_sum_ms += m.tx_creation_ms;
            worker_count += 1;
        }
        if let Some(b) = sequencer_metrics_by_hash.get(hash) {
            let total_ms = b.total_ms.unwrap_or(0.0);
            let decode_ms = b.decode_ms.unwrap_or(0.0);
            let wrap_ms = b.wrap_ms.unwrap_or(0.0);
            let submit_ms = b.submit_ms.unwrap_or(0.0);
            let await_ms = b.await_ms.unwrap_or(0.0);

            sequencer_sum_ms += total_ms;
            seq_decode_sum_ms += decode_ms;
            seq_wrap_sum_ms += wrap_ms;
            seq_submit_sum_ms += submit_ms;
            seq_await_sum_ms += await_ms;
            if let Some(stf) = b.stf_execution_ms {
                seq_stf_sum_ms += stf;
            }
            sequencer_count += 1;
        } else if let Some(ms) = sequencer_times_ms.get(hash) {
            // Only total_ms is available (older sequencer)
            sequencer_sum_ms += *ms;
            sequencer_count += 1;
        }
    }

    // Wait for all new output notes to be indexed before ending the cycle.
    // This ensures the next cycle can find the note commitments for all wallets.
    const NOTE_SYNC_TIMEOUT_SECS: u64 = 30;
    const NOTE_SYNC_POLL_MS: u64 = 100;

    // Collect expected output commitments for wallets that participated in this cycle
    let mut expected_commitments: Vec<([u8; 32], usize)> = Vec::new();
    for input in &inputs {
        // The wallet state has already been updated with the new note secrets.
        let w = &wallets[input.wallet_idx];
        let value_u64: u64 = w
            .value
            .try_into()
            .context("wallet note value does not fit into u64 (required by note_spend_guest v2)")?;
        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &w.spend_sk);
        let recipient = recipient_from_sk_v2(&DOMAIN, &w.spend_sk, &pk_ivk);
        let expected_cm = note_commitment(&DOMAIN, value_u64, &w.rho, &recipient, &w.sender_id);
        expected_commitments.push((expected_cm, input.wallet_idx));
    }

    if !expected_commitments.is_empty() {
        let sync_start = Instant::now();
        let sync_deadline = sync_start + Duration::from_secs(NOTE_SYNC_TIMEOUT_SECS);
        let mut pending: HashSet<[u8; 32]> =
            expected_commitments.iter().map(|(cm, _)| *cm).collect();

        while !pending.is_empty() && Instant::now() < sync_deadline {
            let fresh_positions = fetch_note_positions(client, false).await?;
            pending.retain(|cm| !fresh_positions.contains_key(cm));

            if !pending.is_empty() {
                sleep(Duration::from_millis(NOTE_SYNC_POLL_MS)).await;
            }
        }

        let sync_elapsed = sync_start.elapsed();
        if pending.is_empty() {
            if config.detailed_wallet_logs {
                eprintln!(
                    "[cycle] All {} new output notes indexed in {:.2} ms",
                    expected_commitments.len(),
                    sync_elapsed.as_secs_f64() * 1000.0
                );
            }
            // Update the cached position map with fresh data
            pos_by_cm = fetch_note_positions(client, config.detailed_wallet_logs).await?;
        } else {
            eprintln!(
                "[cycle] WARNING: {} of {} output notes not indexed after {:.2}s timeout",
                pending.len(),
                expected_commitments.len(),
                sync_elapsed.as_secs_f64()
            );
        }
    }

    // Persist rebuilt tree and note index for the next cycle.
    *cached_tree = Some(mt);
    *cached_next_position = cached_next_pos;
    *cached_root = cached_root_val;
    *cached_pos_by_cm = pos_by_cm;

    let avg_worker_ms = if worker_count > 0 {
        worker_sum_ms / worker_count as f64
    } else {
        0.0
    };
    let avg_worker_proof_ms = if worker_count > 0 {
        worker_proof_sum_ms / worker_count as f64
    } else {
        0.0
    };
    let avg_worker_db_ms = if worker_count > 0 {
        worker_db_sum_ms / worker_count as f64
    } else {
        0.0
    };
    let avg_sequencer_ms = if sequencer_count > 0 {
        sequencer_sum_ms / sequencer_count as f64
    } else {
        0.0
    };
    let avg_seq_decode_ms = if sequencer_count > 0 {
        seq_decode_sum_ms / sequencer_count as f64
    } else {
        0.0
    };
    let avg_seq_wrap_ms = if sequencer_count > 0 {
        seq_wrap_sum_ms / sequencer_count as f64
    } else {
        0.0
    };
    let avg_seq_submit_ms = if sequencer_count > 0 {
        seq_submit_sum_ms / sequencer_count as f64
    } else {
        0.0
    };
    let avg_seq_await_ms = if sequencer_count > 0 {
        seq_await_sum_ms / sequencer_count as f64
    } else {
        0.0
    };
    let avg_seq_stf_ms = if sequencer_count > 0 {
        seq_stf_sum_ms / sequencer_count as f64
    } else {
        0.0
    };

    Ok(CycleSummary {
        num_transfers,
        num_included,
        batches,
        avg_worker_ms,
        avg_sequencer_ms,
        avg_worker_proof_ms,
        avg_worker_db_ms,
        avg_seq_decode_ms,
        avg_seq_wrap_ms,
        avg_seq_submit_ms,
        avg_seq_await_ms,
        avg_seq_stf_ms,
    })
}
