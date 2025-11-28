use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use std::sync::Arc;

use chrono::{DateTime, Local};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use demo_stf::runtime::{Runtime, RuntimeCall};
use midnight_privacy::{
    note_commitment, nullifier, CallMessage as MidnightCallMessage, Hash32, MerkleTree, SpendPublic,
};
use rand::Rng;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::json;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_node_client::NodeClient;
use sov_api_spec::types as api_types;
use sov_rollup_interface::crypto::{PrivateKey as _, PublicKey as _};
use sov_rollup_ligero::MockDemoRollup;
use sov_test_utils::default_test_signed_transaction;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

const TREE_DEPTH: u8 = 16;
const TREE_REBUILD_MAX_RETRIES: usize = 3;
const TREE_REBUILD_RETRY_DELAY_MS: u64 = 500;
const MISSING_NOTE_RETRY_MAX: usize = 3;
const MISSING_NOTE_RETRY_DELAY_MS: u64 = 200;
const DOMAIN: [u8; 32] = [1u8; 32];
const NF_KEY: [u8; 32] = [4u8; 32];
const INITIAL_DEPOSIT_AMOUNT: u128 = 100;

#[derive(Clone, Debug)]
struct ContinuousConfig {
    num_wallets: usize,
    initial_deposit: bool,
    per_tx_delay_ms: u64,
    cycle_delay_ms: u64,
    external_node_url: String,
    external_verifier_url: String,
    max_concurrent_proofs: usize,
    detailed_wallet_logs: bool,
    continuous: bool,
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
            .unwrap_or_else(|_| "http://localhost:12346".to_string());
        let external_verifier_url = std::env::var("E2E_ROLLUP_EXTERNAL_VERIFIER_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());

        let max_concurrent_proofs = std::env::var("MAX_CONCURRENT_PROOFS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(num_cpus::get);

        let detailed_wallet_logs = std::env::var("DETAILED_WALLET_LOGS")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let continuous = std::env::var("CONTINUOUS")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

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
        })
    }
}

#[derive(Clone)]
struct WalletState {
    account: PrivateKeyAndAddress<DemoRollupSpec>,
    nonce: u64,
    value: u128,
    rho: Hash32,
    recipient: Hash32,
}

#[derive(Deserialize, Clone)]
struct TreeState {
    root: Vec<u8>,
    next_position: u64,
}

#[derive(Deserialize, Clone)]
struct NoteInfo {
    position: u64,
    commitment: Vec<u8>,
}

#[derive(Deserialize)]
struct NotesResp {
    notes: Vec<NoteInfo>,
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
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
        "[config] node_url={} verifier_url={}",
        config.external_node_url, config.external_verifier_url
    );

    // Setup Ligero environment (program path, prover/verifier bins, shaders)
    let program_path = setup_ligero_env()?;

    let client = NodeClient::new_unchecked(&config.external_node_url);
    let http = HttpClient::new();

    // Fetch chain hash for signing
    #[derive(Deserialize)]
    struct SchemaResp {
        chain_hash: String,
    }
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
    let mut chain_hash = [0u8; 32];
    chain_hash.copy_from_slice(&chain_hash_vec);

    // Load genesis keypairs
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
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

    let node_base_url = config.external_node_url.clone();
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
            recipient: [0u8; 32],
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
    eprintln!(
        "\n[loop] Starting continuous transfer cycles (Ctrl+C to stop)..."
    );
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
            ) => res?,
        };

        total_transfers += summary.num_transfers; total_included += summary.num_included;
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

fn setup_ligero_env() -> Result<String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .ok_or_else(|| anyhow!("Could not find repository root"))?;
    let ligero_dir = repo_root.join("crates/adapters/ligero");
    let platform_dir = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        bail!("Unsupported platform");
    };
    let bin_dir = ligero_dir.join("bins").join(platform_dir).join("bin");
    let shader_dir = ligero_dir.join("bins").join(platform_dir).join("shader");
    let program_path = ligero_dir.join("guest/bins/programs/note_spend_guest.wasm");
    let prover_bin = bin_dir.join("webgpu_prover");
    let verifier_bin = bin_dir.join("webgpu_verifier");

    if !program_path.exists() {
        bail!(
            "note_spend_guest.wasm not found at {}",
            program_path.display()
        );
    }

    std::env::set_var("LIGERO_PROGRAM_PATH", &program_path);
    std::env::set_var("LIGERO_PROVER_BIN", &prover_bin);
    std::env::set_var("LIGERO_VERIFIER_BIN", &verifier_bin);
    std::env::set_var("LIGERO_SHADER_PATH", &shader_dir);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok(program_path.to_string_lossy().to_string())
}

async fn perform_initial_deposits(
    client: &NodeClient,
    http: &HttpClient,
    wallets: &mut [WalletState],
    chain_hash: &[u8; 32],
    per_tx_delay_ms: u64,
    detailed_wallet_logs: bool,
) -> Result<()> {
    for (i, wallet) in wallets.iter_mut().enumerate() {
        let amount: u128 = INITIAL_DEPOSIT_AMOUNT;
        let rho: Hash32 = rand::random();
        let recipient: Hash32 = rand::random();

        let call =
            RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Deposit {
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

        let api_url = &client.base_url;
        let url = format!("{}/sequencer/txs", api_url.trim_end_matches('/'));
        if detailed_wallet_logs {
            eprintln!(
                "[deposit] wallet={} nonce={} amount={} url={}",
                i, wallet.nonce, amount, url
            );
        }

        let resp = http
            .post(&url)
            .json(&json!({ "body": tx_b64 }))
            .send()
            .await
            .context("Deposit HTTP request failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Deposit for wallet {} failed with status {}: {}",
                i,
                status, body
            );
        }

        wallet.nonce += 1;
        wallet.value = amount;
        wallet.rho = rho;
        wallet.recipient = recipient;

        if per_tx_delay_ms > 0 {
            sleep(Duration::from_millis(per_tx_delay_ms)).await;
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

async fn perform_transfer_cycle(
    client: &NodeClient,
    http: &HttpClient,
    wallets: &mut [WalletState],
    chain_hash: &[u8; 32],
    program_path: &str,
    config: &ContinuousConfig,
) -> Result<CycleSummary> {
    // Fetch tree state and all notes with retries in case the sequencer advances while we rebuild the tree
    let (state, all_notes, mt) = {
        let mut attempt_result = None;
        for attempt in 0..TREE_REBUILD_MAX_RETRIES {
            let state_attempt: TreeState = client
                .query_rest_endpoint("/modules/midnight-privacy/tree/state")
                .await
                .context("Failed to query tree state")?;

            let mut all_notes_attempt = Vec::new();
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
                    .with_context(|| {
                        format!("Failed to query notes batch at offset {}", offset)
                    })?;
                let len = batch_resp.notes.len();
                all_notes_attempt.extend(batch_resp.notes);
                if len < batch_size {
                    break;
                }
                offset += batch_size;
            }

            if config.detailed_wallet_logs {
                eprintln!(
                    "[cycle] tree attempt {}: next_position={} notes_count={}",
                    attempt + 1,
                    state_attempt.next_position,
                    all_notes_attempt.len()
                );
            }

            let mut sorted_attempt = all_notes_attempt.clone();
            sorted_attempt.sort_by_key(|n| n.position);

            let mut mt = MerkleTree::new(TREE_DEPTH);
            for n in sorted_attempt.iter() {
                if n.commitment.len() == 32 {
                    let mut cm = [0u8; 32];
                    cm.copy_from_slice(&n.commitment);
                    mt.set_leaf(n.position as usize, cm);
                }
            }
            let rebuilt_root = mt.root();
            if rebuilt_root.as_slice() == state_attempt.root.as_slice() {
                attempt_result = Some((state_attempt, all_notes_attempt, mt));
                break;
            }

            let rebuilt_hex = hex::encode(rebuilt_root);
            let state_hex = hex::encode(&state_attempt.root);

            if config.detailed_wallet_logs {
                eprintln!(
                    "[cycle] tree mismatch attempt {}/{}: rebuilt={} vs state={}",
                    attempt + 1,
                    TREE_REBUILD_MAX_RETRIES,
                    rebuilt_hex,
                    state_hex
                );
            }

            if attempt + 1 == TREE_REBUILD_MAX_RETRIES {
                bail!(
                    "Rebuilt tree root mismatch after {} attempts: rebuilt={} vs state={}",
                    TREE_REBUILD_MAX_RETRIES,
                    rebuilt_hex,
                    state_hex
                );
            }

            sleep(Duration::from_millis(TREE_REBUILD_RETRY_DELAY_MS)).await;
        }

        attempt_result.expect("Tree rebuild attempt must succeed or bail")
    };

    let mut pos_by_cm: HashMap<[u8; 32], u64> = HashMap::new();
    for n in &all_notes {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            pos_by_cm.insert(cm, n.position);
        }
    }

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
        recipient: Hash32,
        position: u64,
    }

    let mut inputs: Vec<TransferInput> = Vec::new();
    for (idx, wallet) in wallets.iter().enumerate() {
        if wallet.value == 0 {
            continue;
        }
        let cm = note_commitment(&DOMAIN, wallet.value, &wallet.rho, &wallet.recipient);
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
                recipient: wallet.recipient,
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
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config.max_concurrent_proofs,
    ));
    let mut proof_tasks = Vec::with_capacity(inputs.len());

    for input in inputs.iter() {
        let account_idx = input.wallet_idx;
        let value = input.value;
        let in_rho = input.rho;
        let in_recipient = input.recipient;
        let position = input.position;
        let siblings = mt.open(position as usize);
        let anchor = anchor_root;
        let program_path = program_path.to_string();
        let sem = semaphore.clone();
        proof_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, Vec<u8>, Hash32, Hash32)> {
                // New output note (same value, fresh rho/recipient)
                let out_rho: Hash32 = rand::thread_rng().gen();
                let out_recipient: Hash32 = rand::thread_rng().gen();
                let cm_out = note_commitment(&DOMAIN, value, &out_rho, &out_recipient);

                let nf = nullifier(&DOMAIN, &NF_KEY, &in_rho);
                let public = SpendPublic {
                    anchor_root: anchor,
                    nullifier: nf,
                    withdraw_amount: 0,
                    output_commitments: vec![cm_out],
                    view_attestations: None,
                };

                let mut private_indices = vec![2, 3, 4, 5, 6];
                for j in 0..depth_usize {
                    private_indices.push(7 + j);
                }
                let out_base = 11 + depth_usize;
                private_indices.extend_from_slice(&[out_base + 0, out_base + 1, out_base + 2]);

                let mut host =
                    <sov_ligero_adapter::Ligero as Zkvm>::Host::from_args(&program_path)
                        .with_private_indices(private_indices);

                host.add_hex_arg(hex::encode(DOMAIN));
                host.add_str_arg(value.to_string());
                host.add_hex_arg(hex::encode(in_rho));
                host.add_hex_arg(hex::encode(in_recipient));
                host.add_hex_arg(hex::encode(NF_KEY));
                host.add_str_arg((position as u64).to_string());
                host.add_str_arg((TREE_DEPTH as u8).to_string());
                for s in &siblings {
                    host.add_hex_arg(hex::encode(s));
                }
                host.add_hex_arg(hex::encode(anchor));
                host.add_hex_arg(hex::encode(nf));
                host.add_str_arg("0".to_string()); // withdraw_amount
                host.add_str_arg("1".to_string()); // ONE output
                host.add_str_arg(value.to_string());
                host.add_hex_arg(hex::encode(out_rho));
                host.add_hex_arg(hex::encode(out_recipient));
                host.add_hex_arg(hex::encode(cm_out));
                host.set_public_output(&public)
                    .context("set public output (round 2)")?;

                let proof_data =
                    host.run(true).context("generate second-round transfer proof")?;
                Ok((account_idx, proof_data, out_rho, out_recipient))
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
        new_recipient: Hash32,
    }

    let mut build_tasks = Vec::with_capacity(proofs.len());
    for (i, (wallet_idx, proof_bytes, out_rho, out_recipient)) in proofs.into_iter().enumerate() {
        let wallet = wallets[wallet_idx].clone();
        let chain_hash = *chain_hash;
        let anchor_root = anchor_root;
        let detailed_logs = config.detailed_wallet_logs;
        build_tasks.push(tokio::task::spawn_blocking(move || -> anyhow::Result<BuiltTransfer> {
            let nf = nullifier(&DOMAIN, &NF_KEY, &wallet.rho);
            let call =
                RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Transfer {
                    proof: <sov_modules_api::SafeVec<u8, 5_000_000>>::try_from(proof_bytes)
                        .map_err(|_| anyhow!("Proof too large for SafeVec"))?,
                    anchor_root,
                    nullifier: nf,
                    view_ciphertexts: None,
                    gas: None,
                });

            let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
                default_test_signed_transaction(
                    &wallet.account.private_key,
                    &call,
                    wallet.nonce,
                    &chain_hash,
                );

            let mut meter = sov_modules_api::gas::UnlimitedGasMeter::<DemoRollupSpec>::default();
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
                new_recipient: out_recipient,
            })
        }));
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
        w.recipient = b.new_recipient;
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
    let submit_endpoint = format!("{}/midnight-privacy", config.external_verifier_url);
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
        .post(format!(
            "{}/midnight-privacy/flush",
            config.external_verifier_url
        ))
        .send()
        .await
        .context("Submit to sequencer request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "Submit to sequencer endpoint returned status {}: {}",
            status, body
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

    let flush: FlushSummary = serde_json::from_str(&body)
        .context("Failed to parse submit to sequencer JSON response")?;
    if config.detailed_wallet_logs {
        eprintln!(
            "[cycle] Submit to sequencer complete. flushed={} accepted={} rejected={} latency_ms={:.2}",
            flush.flushed, flush.accepted, flush.rejected, flush_elapsed_ms
        );
    } else {
        eprintln!("[cycle] Submit to sequencer complete in {:.2} ms (avg {:.2} ms, {:.2} tps)", flush_elapsed_ms, flush_elapsed_ms / flush.flushed as f64, flush.flushed as f64 / (flush_elapsed_ms / 1000.0));
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
                    eprintln!(
                        "    [timing][sequencer] tx={} total_ms={:.2}",
                        hash, ms
                    );
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
        let span_ms = last_instant
            .duration_since(first_instant)
            .as_secs_f64()
            * 1000.0;

        fn format_time_hhmmss_millis(ts: SystemTime) -> String {
            match ts.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(dur) => {
                    DateTime::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos())
                        .map(|dt| dt.with_timezone(&Local))
                        .unwrap_or_else(Local::now)
                        .format("%Y-%m-%d %H:%M:%S%.3f")
                        .to_string()
                }
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
            span_ms, batches.len(), num_included
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
