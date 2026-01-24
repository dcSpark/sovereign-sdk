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
    inv_enforce_v2, nf_key_from_sk, note_commitment, nullifier, pk_from_sk, pk_ivk_from_sk,
    recipient_from_pk_v2, recipient_from_sk_v2, CallMessage as MidnightCallMessage, EncryptedNote,
    Hash32, MerkleTree, PrivacyAddress, SpendPublic,
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
    find_rollup_binary, make_viewer_bundle, setup_ligero_env, start_local_verifier, wait_for_ready,
    ChildGuard, LigeroEnv,
};
use crate::fvk_service::{fetch_viewer_fvk_bundle, ViewerFvkBundle};
use crate::pool_fvk::{ensure_pool_fvk_pk_env, inject_pool_sig_hex_into_proof_bytes};

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

const TREE_DEPTH: u8 = 16;
const TREE_REBUILD_MAX_RETRIES: usize = 5;
const TREE_REBUILD_RETRY_DELAY_MS: u64 = 500;
const MISSING_NOTE_RETRY_MAX: usize = 10;
const MISSING_NOTE_RETRY_DELAY_MS: u64 = 300;
const DOMAIN: [u8; 32] = [1u8; 32];
const INITIAL_DEPOSIT_AMOUNT: u128 = 200;

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

/// Request body for prover service `/prove` endpoint.
#[derive(Clone, serde::Serialize)]
struct ProverServiceRequest {
    circuit: String,
    args: Vec<ligero_runner::LigeroArg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof: Option<String>,
    #[serde(rename = "privateIndices")]
    private_indices: Vec<usize>,
    /// Optional packing size (defaults to 8192 on server)
    #[serde(skip_serializing_if = "Option::is_none")]
    packing: Option<u32>,
}

/// Response body from prover service.
#[derive(Clone, serde::Deserialize)]
struct ProverServiceResponse {
    success: bool,
    #[serde(rename = "exitCode")]
    exit_code: i32,
    proof: Option<String>,
    error: Option<String>,
}


#[derive(Clone, Debug)]
struct ContinuousConfig {
    num_wallets: usize,
    initial_deposit: bool,
    /// Amount to deposit initially into each wallet.
    deposit_amount: u128,
    /// Amount to transfer in each cycle. Defaults to deposit_amount if not set.
    transfer_amount: u128,
    per_tx_delay_ms: u64,
    cycle_delay_ms: u64,
    external_node_url: Option<String>,
    external_verifier_url: Option<String>,
    /// Optional URL of the prover service (e.g., http://127.0.0.1:1313).
    /// When set, proofs are generated via HTTP calls to this service instead of
    /// the local daemon pool. This allows offloading proving to a remote GPU server.
    prover_service_url: Option<String>,
    max_concurrent_proofs: usize,
    detailed_wallet_logs: bool,
    continuous: bool,
    managed_mode: bool,
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

        // Deposit amount configurable via DEPOSIT_AMOUNT env var, defaults to INITIAL_DEPOSIT_AMOUNT (200).
        let deposit_amount = std::env::var("DEPOSIT_AMOUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(INITIAL_DEPOSIT_AMOUNT);

        // Transfer amount defaults to deposit_amount if not specified.
        // Must be <= deposit_amount to ensure sufficient funds.
        let transfer_amount = std::env::var("TRANSFER_AMOUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(deposit_amount);

        if transfer_amount > deposit_amount {
            anyhow::bail!(
                "TRANSFER_AMOUNT ({}) cannot be greater than DEPOSIT_AMOUNT ({})",
                transfer_amount,
                deposit_amount
            );
        }

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

        // Prover service URL for remote proving (defaults to http://127.0.0.1:1313).
        // Set PROVER_SERVICE_URL="" to use local daemon pool instead.
        let prover_service_url = std::env::var("PROVER_SERVICE_URL")
            .ok()
            .map(|v| if v.is_empty() { None } else { Some(v) })
            .unwrap_or_else(|| Some("http://127.0.0.1:1313".to_string()));

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

        let max_cycles = std::env::var("MAX_CYCLES")
            .ok()
            .and_then(|v| v.parse().ok());

        Ok(Self {
            num_wallets,
            initial_deposit,
            deposit_amount,
            transfer_amount,
            per_tx_delay_ms,
            cycle_delay_ms,
            external_node_url,
            external_verifier_url,
            prover_service_url,
            max_concurrent_proofs,
            detailed_wallet_logs,
            continuous,
            managed_mode,
            max_cycles,
        })
    }
}

#[derive(Clone)]
struct WalletState {
    account: PrivateKeyAndAddress<DemoRollupSpec>,
    nonce: u64,
    spend_sk: Hash32,
    notes: Vec<NoteState>,
}

#[derive(Clone, Debug)]
struct NoteState {
    value: u128,
    rho: Hash32,
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
        "[config] wallets={} initial_deposit={} deposit_amount={} transfer_amount={} per_tx_delay_ms={} cycle_delay_ms={} max_concurrent_proofs={}",
        config.num_wallets,
        config.initial_deposit,
        config.deposit_amount,
        config.transfer_amount,
        config.per_tx_delay_ms,
        config.cycle_delay_ms,
        config.max_concurrent_proofs
    );

    // Log if transfer_amount differs from deposit_amount (change notes will be created)
    if config.transfer_amount != config.deposit_amount {
        if config.transfer_amount < config.deposit_amount {
            eprintln!(
                "[config] Note: TRANSFER_AMOUNT ({}) < deposit_amount ({}). Change notes will be created.",
                config.transfer_amount,
                config.deposit_amount
            );
        } else {
            eprintln!(
                "[config] Note: TRANSFER_AMOUNT ({}) > deposit_amount ({}). Transfers will use full wallet balance.",
                config.transfer_amount,
                config.deposit_amount
            );
        }
    }

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
    if let Some(ref url) = config.prover_service_url {
        eprintln!("[config] Prover service: {} (set PROVER_SERVICE_URL=\"\" to use local daemon)", url);
    } else {
        eprintln!("[config] Prover: local daemon pool (PROVER_SERVICE_URL=\"\")");
    }

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
    let http = HttpClient::new();

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

    let client = Arc::new(NodeClient::new_unchecked(&node_url));

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
            spend_sk: [0u8; 32],
            notes: Vec::new(),
        });
    }
    let wallet_setup_ms = wallet_setup_start.elapsed().as_secs_f64() * 1000.0;

    // If pool enforcement is enabled, fetch one viewer FVK per wallet from midnight-fvk-service.
    // Each wallet will include a distinct viewer commitment and pool signature in its proofs.
    let viewer_bundles: Option<Arc<Vec<ViewerFvkBundle>>> = if pool_fvk_pk.is_some() {
        eprintln!(
            "[config] fetching {} viewer FVKs from midnight-fvk-service (1 per wallet)...",
            wallets.len()
        );

        let mut out: Vec<ViewerFvkBundle> = Vec::with_capacity(wallets.len());
        for (i, _w) in wallets.iter().enumerate() {
            let bundle = fetch_viewer_fvk_bundle(&http, pool_fvk_pk).await?;
            if config.detailed_wallet_logs {
                eprintln!(
                    "  [config] viewer wallet={} fvk_commitment=0x{}...",
                    i,
                    hex::encode(&bundle.fvk_commitment[..8])
                );
            }
            out.push(bundle);
        }

        let first = out
            .first()
            .map(|b| format!("0x{}...", hex::encode(&b.fvk_commitment[..8])))
            .unwrap_or_else(|| "<none>".to_string());
        eprintln!(
            "[config] pool viewer enabled: fetched {} FVKs (example commitment={})",
            out.len(),
            first
        );
        Some(Arc::new(out))
    } else {
        eprintln!("[config] pool viewer disabled: transfers will NOT emit viewer ciphertexts");
        None
    };

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
            config.deposit_amount,
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
            config.deposit_amount, deposit_ms
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
                viewer_bundles.clone(),
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
    deposit_amount: u128,
    per_tx_delay_ms: u64,
    detailed_wallet_logs: bool,
) -> Result<()> {
    for (i, wallet) in wallets.iter_mut().enumerate() {
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
        wallet.spend_sk = spend_sk;
        // Deposit convention: sender_id == recipient.
        wallet.notes = vec![NoteState {
            value: amount,
            rho,
            sender_id: recipient,
        }];

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

    let mut expected_commitments: Vec<[u8; 32]> = Vec::new();
    for wallet in wallets.iter() {
        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &wallet.spend_sk);
        let recipient = recipient_from_sk_v2(&DOMAIN, &wallet.spend_sk, &pk_ivk);
        for note in wallet.notes.iter() {
            let value_u64: u64 = note.value.try_into().context(
                "wallet note value does not fit into u64 (required by note_spend_guest v2)",
            )?;
            let cm = note_commitment(
                &DOMAIN,
                value_u64,
                &note.rho,
                &recipient,
                &note.sender_id,
            );
            expected_commitments.push(cm);
        }
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
    viewer_bundles: Option<Arc<Vec<ViewerFvkBundle>>>,
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
    let (_state, _state_root) = {
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

    // Use the verified tree root as anchor. Previously we fetched /roots/recent and picked
    // the last root, but this caused a race condition: if a new block was produced between
    // the tree rebuild and fetching recent_roots, the anchor wouldn't match our tree and
    // Merkle proof validation would fail intermittently.
    let anchor_root: Hash32 = mt.root();

    #[derive(Clone)]
    struct InputNotePlan {
        value: u128,
        rho: Hash32,
        sender_id: Hash32,
        cm: Hash32,
        position: u64,
        siblings: Vec<Hash32>,
    }

    #[derive(Clone)]
    struct TransferPlan {
        sender_idx: usize,
        dest_idx: usize,
        spend_sk: Hash32,
        inputs: Vec<InputNotePlan>,
    }

    let transfer_amount = config.transfer_amount;
    let mut plans: Vec<TransferPlan> = Vec::new();
    for (idx, wallet) in wallets.iter().enumerate() {
        if wallet.notes.is_empty() {
            continue;
        }

        // Select up to 4 notes, largest-first (UTXO consolidation), matching the tx-generator policy.
        let mut selected: Vec<&NoteState> = wallet.notes.iter().collect();
        selected.sort_by(|a, b| b.value.cmp(&a.value).then(b.rho.cmp(&a.rho)));
        selected.truncate(crate::viewer::MAX_INS);

        let total_in: u128 = selected.iter().map(|n| n.value).sum();
        if total_in < transfer_amount {
            if config.detailed_wallet_logs {
                eprintln!(
                    "[cycle] wallet {}: insufficient funds within {} inputs (need {}, have {}); skipping",
                    idx,
                    crate::viewer::MAX_INS,
                    transfer_amount,
                    total_in
                );
            }
            continue;
        }

        // Send to the next wallet (ring) to accumulate multiple notes per wallet over time.
        let dest_idx = if wallets.len() > 1 {
            (idx + 1) % wallets.len()
        } else {
            idx
        };

        let pk_ivk = pk_ivk_from_sk(&DOMAIN, &wallet.spend_sk);
        let recipient = recipient_from_sk_v2(&DOMAIN, &wallet.spend_sk, &pk_ivk);

        // Resolve positions (and open Merkle paths) for all selected inputs.
        let mut cms: Vec<Hash32> = Vec::with_capacity(selected.len());
        for note in selected.iter() {
            let value_u64: u64 = note.value.try_into().context(
                "wallet note value does not fit into u64 (required by note_spend_guest v2)",
            )?;
            let cm = note_commitment(
                &DOMAIN,
                value_u64,
                &note.rho,
                &recipient,
                &note.sender_id,
            );
            cms.push(cm);
        }

        let mut positions: Vec<Option<u64>> = cms.iter().map(|cm| pos_by_cm.get(cm).copied()).collect();
        if positions.iter().any(|p| p.is_none()) {
            // Retry with fresh note fetches and small waits; useful when the tree has just advanced.
            for _attempt in 0..MISSING_NOTE_RETRY_MAX {
                sleep(Duration::from_millis(MISSING_NOTE_RETRY_DELAY_MS)).await;
                pos_by_cm = fetch_note_positions(client, config.detailed_wallet_logs).await?;
                positions = cms.iter().map(|cm| pos_by_cm.get(cm).copied()).collect();
                if positions.iter().all(|p| p.is_some()) {
                    break;
                }
            }
        }

        if positions.iter().any(|p| p.is_none()) {
            eprintln!(
                "[cycle] wallet {}: some input commitments not yet in tree; skipping this cycle",
                idx
            );
            continue;
        }

        let mut inputs: Vec<InputNotePlan> = Vec::with_capacity(selected.len());
        let mut tree_mismatch = false;
        for ((note, cm), pos_opt) in selected.iter().zip(cms.iter()).zip(positions.into_iter()) {
            let position = pos_opt.expect("checked above");
            let tree_leaf = mt.leaf(position as usize);
            if tree_leaf != *cm {
                eprintln!(
                    "[cycle] wallet {}: TREE MISMATCH! position={} expected_cm={} tree_leaf={}",
                    idx,
                    position,
                    hex::encode(&cm[..8]),
                    hex::encode(&tree_leaf[..8])
                );
                tree_mismatch = true;
                break;
            }
            let siblings = mt.open(position as usize);
            inputs.push(InputNotePlan {
                value: note.value,
                rho: note.rho,
                sender_id: note.sender_id,
                cm: *cm,
                position,
                siblings,
            });
        }
        if tree_mismatch {
            continue;
        }

        plans.push(TransferPlan {
            sender_idx: idx,
            dest_idx,
            spend_sk: wallet.spend_sk,
            inputs,
        });
    }

    if plans.is_empty() {
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
        plans.len(),
        config.max_concurrent_proofs
    );
    let proof_generation_start = Instant::now();

    use sov_rollup_interface::zk::{Zkvm, ZkvmHost};

    /// Result from proof generation task containing note secrets for wallet update.
    #[derive(Clone)]
    struct ProofResult {
        sender_idx: usize,
        dest_idx: usize,
        proof_data: Vec<u8>,
        nullifiers: Vec<Hash32>,
        input_cms: Vec<Hash32>,
        spent_rhos: Vec<Hash32>,
        sender_id_out: Hash32,
        /// Pay note: sent to `dest_idx`
        pay_value: u128,
        pay_rho: Hash32,
        /// Change note (if any): same owner as input, fresh rho
        change_value: u128,
        change_rho: Option<Hash32>,
    }

    let depth_usize = TREE_DEPTH as usize;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_proofs));
    let mut proof_tasks = Vec::with_capacity(plans.len());

    let viewer_bundles = viewer_bundles.clone();
    let prover_service_url = config.prover_service_url.clone();
    let transfer_amount = config.transfer_amount;
    for plan in plans.iter() {
        let sender_idx = plan.sender_idx;
        let dest_idx = plan.dest_idx;
        let in_spend_sk = plan.spend_sk;
        let input_notes = plan.inputs.clone();
        let dest_spend_sk = wallets[dest_idx].spend_sk;
        let anchor = anchor_root;
        let program_path = program_path.to_string();
        let sem = semaphore.clone();
        let viewer_bundles = viewer_bundles.clone();
        let daemon_workers = config.max_concurrent_proofs;
        let client = client.clone();
        let prover_service_url = prover_service_url.clone();
        proof_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            // Calculate pay and change amounts
            let total_in: u128 = input_notes.iter().map(|n| n.value).sum();
            anyhow::ensure!(
                total_in >= transfer_amount,
                "wallet {}: insufficient funds (need {}, have {})",
                sender_idx,
                transfer_amount,
                total_in
            );
            let pay_value = transfer_amount;
            let change_value = total_in - pay_value;
            let has_change = change_value > 0;

            // Fetch deny-map openings (sender + pay recipient + change recipient if needed).
            // The spend circuit binds to the current `blacklist_root` as a public input and requires,
            // for each checked id:
            // - bucket_entries[BLACKLIST_BUCKET_SIZE] (private)
            // - bucket_inv (private)
            // - siblings[BLACKLIST_TREE_DEPTH] (private)
            let pk_ivk_owner = pk_ivk_from_sk(&DOMAIN, &in_spend_sk);
            let pk_spend_owner = pk_from_sk(&in_spend_sk);
            let sender_addr = PrivacyAddress::from_keys(&pk_spend_owner, &pk_ivk_owner);

            // Pay output note parameters (destination wallet)
            let pay_rho: Hash32 = rand::thread_rng().gen();
            let pay_pk_spend = pk_from_sk(&dest_spend_sk);
            let pay_pk_ivk = pk_ivk_from_sk(&DOMAIN, &dest_spend_sk);
            let pay_addr = PrivacyAddress::from_keys(&pay_pk_spend, &pay_pk_ivk);

            // Change output note parameters (same owner as input, fresh rho)
            let change_rho: Hash32 = rand::thread_rng().gen();
            // Change note goes back to sender (same keys)
            let change_pk_spend = pk_spend_owner;
            let change_pk_ivk = pk_ivk_owner;

            // Fetch blacklist openings for sender and pay recipient only.
            // Change recipient is enforced to be self (sender) in-circuit, no separate check needed.
            let (sender_opening, pay_opening) = tokio::try_join!(
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
                            "/modules/midnight-privacy/blacklist/opening/{pay_addr}"
                        ))
                        .await
                }
            )
            .context("Failed to query deny-map openings")?;

            anyhow::ensure!(
                sender_opening.blacklist_root == pay_opening.blacklist_root,
                "Deny-map root changed while fetching openings (sender vs pay)"
            );
            let blacklist_root = sender_opening.blacklist_root;

            if sender_opening.is_blacklisted {
                anyhow::bail!("Sender privacy address is frozen (blacklisted)");
            }
            if pay_opening.is_blacklisted {
                anyhow::bail!("Pay recipient privacy address is frozen (blacklisted)");
            }
            // Change recipient uses sender's address, already checked above

            let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
            anyhow::ensure!(
                sender_opening.siblings.len() == bl_depth,
                "sender deny-map opening has wrong sibling length: got {}, expected {}",
                sender_opening.siblings.len(),
                bl_depth
            );
            anyhow::ensure!(
                pay_opening.siblings.len() == bl_depth,
                "pay recipient deny-map opening has wrong sibling length: got {}, expected {}",
                pay_opening.siblings.len(),
                bl_depth
            );
            let sender_bl_bucket_entries = sender_opening.bucket_entries;
            let sender_bl_siblings = sender_opening.siblings;
            let pay_bl_bucket_entries = pay_opening.bucket_entries;
            let pay_bl_siblings = pay_opening.siblings;

            tokio::task::spawn_blocking(
                move || -> anyhow::Result<ProofResult> {
                    let (viewer_fvk, pool_sig_hex) = if let Some(ref bundles) = viewer_bundles {
                        let b = bundles.get(sender_idx).ok_or_else(|| {
                            anyhow!(
                                "missing viewer bundle for wallet {sender_idx} (have {} bundles)",
                                bundles.len()
                            )
                        })?;
                        (Some(b.fvk), Some(b.pool_sig_hex.clone()))
                    } else {
                        (None, None)
                    };
                    let n_in = input_notes.len();
                    anyhow::ensure!(n_in > 0, "n_in must be > 0");
                    anyhow::ensure!(
                        n_in <= crate::viewer::MAX_INS,
                        "n_in must be <= {}",
                        crate::viewer::MAX_INS
                    );

                    let mut in_values_u64: Vec<u64> = Vec::with_capacity(n_in);
                    let mut in_rhos: Vec<Hash32> = Vec::with_capacity(n_in);
                    let mut in_sender_ids: Vec<Hash32> = Vec::with_capacity(n_in);
                    let mut positions: Vec<u64> = Vec::with_capacity(n_in);
                    let mut siblings_by_input: Vec<Vec<Hash32>> = Vec::with_capacity(n_in);
                    let mut input_cms: Vec<Hash32> = Vec::with_capacity(n_in);
                    let mut spent_rhos: Vec<Hash32> = Vec::with_capacity(n_in);

                    for n in input_notes.iter() {
                        let v_u64: u64 = n.value.try_into().context(
                            "note value does not fit into u64 (required by note_spend_guest v2)",
                        )?;
                        if v_u64 > i64::MAX as u64 {
                            bail!("note value does not fit into i64 (required by note_spend_guest v2 ABI)");
                        }
                        in_values_u64.push(v_u64);
                        in_rhos.push(n.rho);
                        in_sender_ids.push(n.sender_id);
                        positions.push(n.position);
                        siblings_by_input.push(n.siblings.clone());
                        input_cms.push(n.cm);
                        spent_rhos.push(n.rho);
                    }

                    anyhow::ensure!(
                        siblings_by_input.iter().all(|s| s.len() == depth_usize),
                        "Merkle path depth mismatch (expected {} siblings per input)",
                        depth_usize
                    );

                    let pay_value_u64: u64 = pay_value.try_into().context("pay value does not fit into u64")?;
                    let change_value_u64: u64 =
                        change_value.try_into().context("change value does not fit into u64")?;

                    // note_spend_guest v2 derives the owner recipient from (spend_sk, pk_ivk_owner).
                    let in_recipient = recipient_from_sk_v2(&DOMAIN, &in_spend_sk, &pk_ivk_owner);
                    let sender_id_out = in_recipient;

                    // Pay output note (goes to destination wallet)
                    let pay_recipient = recipient_from_pk_v2(&DOMAIN, &pay_pk_spend, &pay_pk_ivk);
                    let cm_pay =
                        note_commitment(&DOMAIN, pay_value_u64, &pay_rho, &pay_recipient, &sender_id_out);

                    // Change output note (goes back to owner)
                    let change_recipient = if has_change {
                        recipient_from_pk_v2(&DOMAIN, &change_pk_spend, &change_pk_ivk)
                    } else {
                        [0u8; 32] // unused
                    };
                    let cm_change = if has_change {
                        note_commitment(
                            &DOMAIN,
                            change_value_u64,
                            &change_rho,
                            &change_recipient,
                            &sender_id_out,
                        )
                    } else {
                        [0u8; 32] // unused
                    };

                    // Nullifiers are derived from spend_sk (nf_key is derived inside the circuit).
                    let nf_key = nf_key_from_sk(&DOMAIN, &in_spend_sk);
                    let nullifiers: Vec<Hash32> =
                        in_rhos.iter().map(|rho| nullifier(&DOMAIN, &nf_key, rho)).collect();

                    // Build viewer attestations if pool viewer is configured.
                    // The circuit expects: n_viewers=1, fvk_commitment, fvk, then ct_hash+mac for EACH output.
                    // So view_attestations should include attestations for ALL outputs (pay + change if applicable).
                    let n_out: usize = if has_change { 2 } else { 1 };
                    let (view_attestations, viewer_data_list) = if let Some(fvk) = viewer_fvk {
                        let mut cm_ins: [Hash32; crate::viewer::MAX_INS] =
                            [[0u8; 32]; crate::viewer::MAX_INS];
                        for (i, cm) in input_cms.iter().enumerate().take(crate::viewer::MAX_INS) {
                            cm_ins[i] = *cm;
                        }

                        let (pay_att, _pay_enc) = make_viewer_bundle(
                            &fvk,
                            &DOMAIN,
                            pay_value,
                            &pay_rho,
                            &pay_recipient,
                            &sender_id_out,
                            &cm_ins,
                            &cm_pay,
                        )?;
                        if has_change {
                            let (change_att, _change_enc) = make_viewer_bundle(
                                &fvk,
                                &DOMAIN,
                                change_value,
                                &change_rho,
                                &change_recipient,
                                &sender_id_out,
                                &cm_ins,
                                &cm_change,
                            )?;
                            // Include attestations for BOTH outputs
                            (
                                Some(vec![pay_att.clone(), change_att.clone()]),
                                Some(vec![(fvk, pay_att), (fvk, change_att)]),
                            )
                        } else {
                            (Some(vec![pay_att.clone()]), Some(vec![(fvk, pay_att)]))
                        }
                    } else {
                        (None, None)
                    };

                let output_commitments = if has_change {
                    vec![cm_pay, cm_change]
                } else {
                    vec![cm_pay]
                };

                let public = SpendPublic {
                    anchor_root: anchor,
                    blacklist_root,
                    nullifiers: nullifiers.clone(),
                    withdraw_amount: 0,
                    output_commitments,
                    view_attestations,
                };

                // LigeroConfig private indices are 1-based (by argument position).
                let mut private_indices: Vec<usize> = Vec::new();
                private_indices.extend_from_slice(&[2, 3]); // spend_sk, pk_ivk_owner

                let per_in = 5usize + depth_usize;
                let withdraw_idx = 7usize + n_in * per_in;
                let outs_base = withdraw_idx + 3;

                // Input private args (per input: value, rho, sender_id, pos, siblings[depth])
                for in_idx in 0..n_in {
                    let base = 7usize + in_idx * per_in;
                    private_indices.extend_from_slice(&[
                        base,         // value_in
                        base + 1,     // rho_in
                        base + 2,     // sender_id_in
                        base + 3,     // pos
                    ]);
                    // siblings start at base+4
                    for j in 0..depth_usize {
                        private_indices.push(base + 4 + j);
                    }
                }

                // Output private args (5 args per output: value, rho, pk_spend, pk_ivk, commitment)
                // value_out, rho_out, pk_spend_out, pk_ivk_out are private; commitment is public
                for out_idx in 0..n_out {
                    let out_base = outs_base + out_idx * 5;
                    private_indices.extend_from_slice(&[
                        out_base,         // value_out
                        out_base + 1,     // rho_out
                        out_base + 2,     // pk_spend_out
                        out_base + 3,     // pk_ivk_out
                    ]);
                }
                // inv_enforce (private)
                let inv_enforce_idx = outs_base + 5 * n_out;
                private_indices.push(inv_enforce_idx);

                // Deny-map (blacklist) section:
                // - blacklist_root is PUBLIC (comes right after inv_enforce)
                // - for each checked id: bucket_entries[BLACKLIST_BUCKET_SIZE] + bucket_inv + siblings[BLACKLIST_TREE_DEPTH]
                // Note: Only 2 checks (sender + pay recipient). Change outputs are enforced to be self in-circuit.
                let bl_root_idx = inv_enforce_idx + 1;
                let bl_args_start = bl_root_idx + 1;
                let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
                let bl_per_check =
                    midnight_privacy::BLACKLIST_BUCKET_SIZE + 1usize + bl_depth;
                let bl_checks = 2usize; // sender_id + pay recipient (change is enforced to be self in-circuit)
                for j in 0..(bl_checks * bl_per_check) {
                    private_indices.push(bl_args_start + j);
                }

                // Viewer section (Level B): n_viewers=1, fvk_commitment, fvk, then ct_hash+mac for each output
                let n_viewers_idx = bl_args_start + bl_checks * bl_per_check;
                let fvk_commitment_arg_pos = if viewer_data_list.is_some() {
                    Some(n_viewers_idx + 1)
                } else {
                    None
                };
                if viewer_data_list.is_some() {
                    // FVK is private (at position n_viewers_idx + 2)
                    private_indices.push(n_viewers_idx + 2);
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
                host.add_u64_arg(n_in as u64); // 6: n_in (PUBLIC)

                for i in 0..n_in {
                    host.add_u64_arg(in_values_u64[i]); // value_in_i (PRIVATE)
                    host.add_hex_arg(hex::encode(in_rhos[i])); // rho_in_i (PRIVATE)
                    host.add_hex_arg(hex::encode(in_sender_ids[i])); // sender_id_in_i (PRIVATE)
                    host.add_u64_arg(positions[i] as u64); // pos_i (PRIVATE)
                    for s in siblings_by_input[i].iter() {
                        host.add_hex_arg(hex::encode(s));
                    }
                    host.add_hex_arg(hex::encode(nullifiers[i])); // nullifier_i (PUBLIC)
                }

                host.add_u64_arg(0); // withdraw_amount (PUBLIC)
                host.add_hex_arg(hex::encode([0u8; 32])); // withdraw_to (PUBLIC; must be 0 for transfers)
                host.add_u64_arg(n_out as u64); // n_out (PUBLIC)

                // Pay output (output 0)
                host.add_u64_arg(pay_value_u64);
                host.add_hex_arg(hex::encode(pay_rho));
                host.add_hex_arg(hex::encode(pay_pk_spend));
                host.add_hex_arg(hex::encode(pay_pk_ivk));
                host.add_hex_arg(hex::encode(cm_pay));

                // Change output (output 1) if applicable
                if has_change {
                    host.add_u64_arg(change_value_u64);
                    host.add_hex_arg(hex::encode(change_rho));
                    host.add_hex_arg(hex::encode(change_pk_spend));
                    host.add_hex_arg(hex::encode(change_pk_ivk));
                    host.add_hex_arg(hex::encode(cm_change));
                }

                // inv_enforce (PRIVATE) - use the canonical formula from midnight_privacy
                // Formula: Π(in_values) * Π(out_values) * Π(out_rho - in_rho)
                let (out_values, out_rhos): (Vec<u64>, Vec<Hash32>) = if has_change {
                    (vec![pay_value_u64, change_value_u64], vec![pay_rho, change_rho])
                } else {
                    (vec![pay_value_u64], vec![pay_rho])
                };
                let inv_enforce = inv_enforce_v2(
                    &in_values_u64, // in_values
                    &in_rhos,       // in_rhos
                    &out_values,    // out_values
                    &out_rhos,      // out_rhos
                );
                host.add_hex_arg(hex::encode(inv_enforce));

                // Deny-map (blacklist) args:
                //   blacklist_root (PUBLIC)
                //   sender check: bucket_entries[12] (PRIVATE) + bucket_inv (PRIVATE) + siblings[16] (PRIVATE)
                //   pay recipient check: bucket_entries[12] (PRIVATE) + bucket_inv (PRIVATE) + siblings[16] (PRIVATE)
                //   change recipient check (if applicable): bucket_entries[12] (PRIVATE) + bucket_inv (PRIVATE) + siblings[16] (PRIVATE)
                let bucket_inv_for_id =
                    |id: &Hash32, bucket_entries: &[Hash32]| -> anyhow::Result<Hash32> {
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
                // Sender check
                for e in &sender_bl_bucket_entries {
                    host.add_hex_arg(hex::encode(e));
                }
                let sender_bucket_inv =
                    bucket_inv_for_id(&sender_id_out, &sender_bl_bucket_entries)?;
                host.add_hex_arg(hex::encode(sender_bucket_inv));
                for sib in sender_bl_siblings.iter().take(bl_depth) {
                    host.add_hex_arg(hex::encode(sib));
                }
                // Pay recipient check
                for e in &pay_bl_bucket_entries {
                    host.add_hex_arg(hex::encode(e));
                }
                let pay_bucket_inv = bucket_inv_for_id(&pay_recipient, &pay_bl_bucket_entries)?;
                host.add_hex_arg(hex::encode(pay_bucket_inv));
                for sib in pay_bl_siblings.iter().take(bl_depth) {
                    host.add_hex_arg(hex::encode(sib));
                }
                // Note: Change recipient blacklist check is NOT needed - the circuit enforces
                // that change outputs go back to the sender (self) in-circuit.

                // Viewer section (Level-B)
                // Structure: n_viewers=1, fvk_commitment, fvk, then ct_hash+mac for EACH output
                if let Some(ref data_list) = viewer_data_list {
                    host.add_u64_arg(1u64); // n_viewers = 1 (number of distinct FVKs)
                    // Use the first attestation for fvk_commitment and fvk
                    if let Some((fvk, att)) = data_list.first() {
                        host.add_hex_arg(hex::encode(att.fvk_commitment));
                        host.add_hex_arg(hex::encode(fvk));
                    }
                    // Add ct_hash + mac for EACH output
                    for (_fvk, att) in data_list.iter().take(n_out) {
                        host.add_hex_arg(hex::encode(att.ct_hash));
                        host.add_hex_arg(hex::encode(att.mac));
                    }
                }

                host.set_public_output(&public)
                    .context("set public output (round 2)")?;

                // Generate proof via either prover service (HTTP) or local daemon pool.
                let proof_data = if let Some(ref service_url) = prover_service_url {
                    // Use remote prover service via blocking HTTP call.
                    (|| -> anyhow::Result<Vec<u8>> {
                        let public_output = host.require_public_output()?;
                        let cfg = host.runner().config().clone();
                        let args = cfg.args.clone();
                        let private_indices = cfg.private_indices.clone();

                        // Resolve program path to ensure consistency with what the HTTP server expects.
                        // The server's resolve_circuit handles names like "note_spend_guest".
                        let circuit_name = cfg.program.clone();

                        // Create blocking HTTP client for the prover service call.
                        let blocking_client = reqwest::blocking::Client::new();
                        let url = format!("{}/prove", service_url.trim_end_matches('/'));

                        let request = ProverServiceRequest {
                            circuit: circuit_name,
                            args: args.clone(),
                            proof: None,
                            private_indices: private_indices.clone(),
                            packing: Some(cfg.packing),
                        };

                        // Retry logic for transient failures (timeouts, connection errors)
                        const MAX_RETRIES: u32 = 3;
                        const RETRY_DELAY_MS: u64 = 2000;

                        let mut last_error: Option<anyhow::Error> = None;
                        let mut body: Option<ProverServiceResponse> = None;

                        for attempt in 1..=MAX_RETRIES {
                            match blocking_client.post(&url).json(&request).send() {
                                Ok(resp) => {
                                    let status = resp.status();
                                    match resp.json::<ProverServiceResponse>() {
                                        Ok(parsed) => {
                                            if !status.is_success() || !parsed.success {
                                                last_error = Some(anyhow::anyhow!(
                                                    "Prover service returned error (status={}, exit_code={}): {}",
                                                    status,
                                                    parsed.exit_code,
                                                    parsed.error.clone().unwrap_or_else(|| "unknown error".to_string())
                                                ));
                                                // Don't retry on application-level errors
                                                body = Some(parsed);
                                                break;
                                            }
                                            body = Some(parsed);
                                            last_error = None;
                                            break;
                                        }
                                        Err(e) => {
                                            last_error = Some(anyhow::anyhow!("Failed to parse prover service response: {}", e));
                                            // Don't retry parse errors
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    last_error = Some(anyhow::anyhow!("Failed to send request to prover service: {}", e));
                                    if attempt < MAX_RETRIES {
                                        eprintln!(
                                            "[warn] Prover service request failed (attempt {}/{}): {}. Retrying in {}ms...",
                                            attempt,
                                            MAX_RETRIES,
                                            e,
                                            RETRY_DELAY_MS
                                        );
                                        std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
                                    }
                                }
                            }
                        }

                        if let Some(err) = last_error {
                            return Err(err);
                        }

                        let body = body.ok_or_else(|| anyhow::anyhow!("No response from prover service after retries"))?;

                        let proof_b64 = body
                            .proof
                            .ok_or_else(|| anyhow::anyhow!("Prover service returned success but no proof"))?;

                        let proof_bytes = BASE64_STANDARD
                            .decode(&proof_b64)
                            .context("Failed to decode base64 proof from prover service")?;

                        let args_json = serde_json::to_vec(&args)?;
                        let pkg = ligero_runner::LigeroProofPackage::new(
                            proof_bytes,
                            public_output,
                            args_json,
                            private_indices,
                        )?;
                        Ok(bincode::serialize(&pkg)?)
                    })()
                    .context("generate transfer proof via prover service")?
                } else {
                    // Daemon-mode prover ONLY: keep webgpu_prover warm and avoid respawning for each proof.
                    (|| -> anyhow::Result<Vec<u8>> {
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

                        let proof_bytes = std::fs::read(&proof_path)
                            .with_context(|| format!("failed to read proof at {}", proof_path.display()))?;
                        drop(tmp);

                        let args_json = serde_json::to_vec(&cfg.args)?;
                        let pkg = ligero_runner::LigeroProofPackage::new(
                            proof_bytes,
                            public_output,
                            args_json,
                            cfg.private_indices.clone(),
                        )?;
                        Ok(bincode::serialize(&pkg)?)
                    })()
                    .context("generate transfer proof via daemon")?
                };

                let proof_data = if let Some(pool_sig_hex) = pool_sig_hex.as_ref() {
                    if viewer_data_list.is_none() {
                        bail!("POOL_FVK_PK is set but viewer section is missing in transfer proof args");
                    }
                    let fvk_commitment_arg_pos = fvk_commitment_arg_pos.ok_or_else(|| {
                        anyhow!(
                            "POOL_FVK_PK is set but fvk_commitment_arg_pos is missing (viewer section not enabled)"
                        )
                    })?;
                    inject_pool_sig_hex_into_proof_bytes(
                        proof_data,
                        fvk_commitment_arg_pos,
                        pool_sig_hex.clone(),
                    )?
                } else {
                    proof_data
                };
                Ok(ProofResult {
                    sender_idx,
                    dest_idx,
                    proof_data,
                    nullifiers,
                    input_cms,
                    spent_rhos,
                    sender_id_out,
                    pay_value,
                    pay_rho,
                    change_value,
                    change_rho: if has_change { Some(change_rho) } else { None },
                })
                },
            )
            .await
            .expect("spawn_blocking join failed")
        }));
    }

    let mut proofs: Vec<ProofResult> = Vec::with_capacity(plans.len());
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
        sender_idx: usize,
        dest_idx: usize,
        tx_hash: String,
        tx_b64: String,
        new_nonce: u64,
        spent_rhos: Vec<Hash32>,
        pay_note: NoteState,
        change_note: Option<NoteState>,
    }

    let mut build_tasks = Vec::with_capacity(proofs.len());
    for (i, proof_result) in proofs.into_iter().enumerate() {
        let sender_idx = proof_result.sender_idx;
        let dest_idx = proof_result.dest_idx;
        let proof_bytes = proof_result.proof_data;
        let nullifiers = proof_result.nullifiers;
        let input_cms = proof_result.input_cms;
        let spent_rhos = proof_result.spent_rhos;
        let sender_id_out = proof_result.sender_id_out;
        let pay_value = proof_result.pay_value;
        let pay_rho = proof_result.pay_rho;
        let change_value = proof_result.change_value;
        let change_rho = proof_result.change_rho;
        let has_change = change_rho.is_some();

        let sender_wallet = wallets[sender_idx].clone();
        let dest_spend_sk = wallets[dest_idx].spend_sk;
        let chain_hash = *chain_hash;
        let anchor_root = anchor_root;
        let detailed_logs = config.detailed_wallet_logs;
        let viewer_fvk = if let Some(ref bundles) = viewer_bundles {
            let b = bundles.get(sender_idx).ok_or_else(|| {
                anyhow!(
                    "missing viewer bundle for wallet {sender_idx} (have {} bundles)",
                    bundles.len()
                )
            })?;
            Some(b.fvk)
        } else {
            None
        };
        build_tasks.push(tokio::task::spawn_blocking(
            move || -> anyhow::Result<BuiltTransfer> {
                let pay_value_u64: u64 = pay_value.try_into().context(
                    "pay value does not fit into u64 (required by note_spend_guest v2)",
                )?;

                // Pay output goes to destination wallet
                let pay_pk_spend = pk_from_sk(&dest_spend_sk);
                let pay_pk_ivk = pk_ivk_from_sk(&DOMAIN, &dest_spend_sk);
                let pay_recipient = recipient_from_pk_v2(&DOMAIN, &pay_pk_spend, &pay_pk_ivk);

                let pk_ivk_owner = pk_ivk_from_sk(&DOMAIN, &sender_wallet.spend_sk);
                let pk_spend_owner = pk_from_sk(&sender_wallet.spend_sk);

                // Build encrypted notes for pool viewer if configured.
                // Encrypt both pay output and change output (if applicable).
                let view_ciphertexts: Option<Vec<EncryptedNote>> = match viewer_fvk {
                    Some(fvk) => {
                        let mut cm_ins: [Hash32; crate::viewer::MAX_INS] =
                            [[0u8; 32]; crate::viewer::MAX_INS];
                        for (i, cm) in input_cms.iter().enumerate().take(crate::viewer::MAX_INS) {
                            cm_ins[i] = *cm;
                        }
                        let mut ciphertexts = Vec::new();
                        
                        // Pay note ciphertext
                        let cm_pay = note_commitment(
                            &DOMAIN,
                            pay_value_u64,
                            &pay_rho,
                            &pay_recipient,
                            &sender_id_out,
                        );
                        let (_att, enc) = make_viewer_bundle(
                            &fvk,
                            &DOMAIN,
                            pay_value,
                            &pay_rho,
                            &pay_recipient,
                            &sender_id_out,
                            &cm_ins,
                            &cm_pay,
                        )?;
                        ciphertexts.push(enc);

                        // Change note ciphertext (if applicable)
                        if let Some(change_rho_val) = change_rho {
                            let change_value_u64: u64 = change_value.try_into().context(
                                "change value does not fit into u64",
                            )?;
                            let change_recipient =
                                recipient_from_pk_v2(&DOMAIN, &pk_spend_owner, &pk_ivk_owner);
                            let cm_change = note_commitment(
                                &DOMAIN,
                                change_value_u64,
                                &change_rho_val,
                                &change_recipient,
                                &sender_id_out,
                            );
                            let (_att, enc) = make_viewer_bundle(
                                &fvk,
                                &DOMAIN,
                                change_value,
                                &change_rho_val,
                                &change_recipient,
                                &sender_id_out,
                                &cm_ins,
                                &cm_change,
                            )?;
                            ciphertexts.push(enc);
                        }

                        Some(ciphertexts)
                    }
                    None => None,
                };

                let call =
                    RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Transfer {
                        proof: proof_bytes
                            .try_into()
                            .map_err(|_| anyhow!("Proof too large for SafeVec"))?,
                        anchor_root,
                        nullifiers: nullifiers.clone(),
                        view_ciphertexts,
                        gas: None,
                    });

                let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
                    default_test_signed_transaction(
                        &sender_wallet.account.private_key,
                        &call,
                        sender_wallet.nonce,
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
                    if has_change {
                        eprintln!(
                            "  [transfer] wallet={} idx_in_cycle={} nonce={} tx={} n_in={} nullifier0={} pay={} change={}",
                            sender_idx,
                            i + 1,
                            sender_wallet.nonce,
                            tx_hash,
                            nullifiers.len(),
                            hex::encode(&nullifiers[0][..8]),
                            pay_value,
                            change_value
                        );
                    } else {
                        eprintln!(
                            "  [transfer] wallet={} idx_in_cycle={} nonce={} tx={} n_in={} nullifier0={} value={}",
                            sender_idx,
                            i + 1,
                            sender_wallet.nonce,
                            tx_hash,
                            nullifiers.len(),
                            hex::encode(&nullifiers[0][..8]),
                            pay_value
                        );
                    }
                }

                let pay_note = NoteState {
                    value: pay_value,
                    rho: pay_rho,
                    sender_id: sender_id_out,
                };
                let change_note = change_rho.map(|rho| NoteState {
                    value: change_value,
                    rho,
                    sender_id: sender_id_out,
                });

                Ok(BuiltTransfer {
                    idx: i,
                    sender_idx,
                    dest_idx,
                    tx_hash,
                    tx_b64,
                    new_nonce: sender_wallet.nonce + 1,
                    spent_rhos,
                    pay_note,
                    change_note,
                })
            },
        ));
    }

    let mut built = Vec::with_capacity(build_tasks.len());
    for t in build_tasks {
        built.push(t.await??);
    }
    built.sort_by_key(|b| b.idx);

    let mut expected_output_commitments: Vec<Hash32> = Vec::new();
    for b in built {
        transfer_hashes.push(b.tx_hash);
        transfer_txs_b64.push((b.sender_idx, b.tx_b64));

        // Track expected output commitments so the next cycle can find them in the tree.
        let pay_value_u64: u64 = b
            .pay_note
            .value
            .try_into()
            .context("pay note value does not fit into u64")?;
        let dest_spend_sk = wallets[b.dest_idx].spend_sk;
        let pay_pk_spend = pk_from_sk(&dest_spend_sk);
        let pay_pk_ivk = pk_ivk_from_sk(&DOMAIN, &dest_spend_sk);
        let pay_recipient = recipient_from_pk_v2(&DOMAIN, &pay_pk_spend, &pay_pk_ivk);
        let cm_pay = note_commitment(
            &DOMAIN,
            pay_value_u64,
            &b.pay_note.rho,
            &pay_recipient,
            &b.pay_note.sender_id,
        );
        expected_output_commitments.push(cm_pay);

        if let Some(ref change) = b.change_note {
            let change_value_u64: u64 = change
                .value
                .try_into()
                .context("change note value does not fit into u64")?;
            let sender_spend_sk = wallets[b.sender_idx].spend_sk;
            let change_pk_spend = pk_from_sk(&sender_spend_sk);
            let change_pk_ivk = pk_ivk_from_sk(&DOMAIN, &sender_spend_sk);
            let change_recipient = recipient_from_pk_v2(&DOMAIN, &change_pk_spend, &change_pk_ivk);
            let cm_change = note_commitment(
                &DOMAIN,
                change_value_u64,
                &change.rho,
                &change_recipient,
                &change.sender_id,
            );
            expected_output_commitments.push(cm_change);
        }

        // Update sender wallet: consume inputs and add change (if any).
        {
            let w = &mut wallets[b.sender_idx];
            w.nonce = b.new_nonce;
            let spent: HashSet<Hash32> = b.spent_rhos.iter().copied().collect();
            w.notes.retain(|n| !spent.contains(&n.rho));
            if let Some(change) = b.change_note {
                w.notes.push(change);
            }
        }

        // Update destination wallet: add the pay note.
        wallets[b.dest_idx].notes.push(b.pay_note);
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

    // Wait for all new output notes to be indexed before ending the cycle.
    // This ensures the next cycle can find the note commitments.
    if !expected_output_commitments.is_empty() {
        let sync_start = Instant::now();
        let sync_deadline = sync_start + Duration::from_secs(NOTE_SYNC_TIMEOUT_SECS);
        let mut pending: HashSet<Hash32> = expected_output_commitments.iter().copied().collect();

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
                    expected_output_commitments.len(),
                    sync_elapsed.as_secs_f64() * 1000.0
                );
            }
            // Update the cached position map with fresh data
            pos_by_cm = fetch_note_positions(client, config.detailed_wallet_logs).await?;
        } else {
            eprintln!(
                "[cycle] WARNING: {} of {} output notes not indexed after {:.2}s timeout",
                pending.len(),
                expected_output_commitments.len(),
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
