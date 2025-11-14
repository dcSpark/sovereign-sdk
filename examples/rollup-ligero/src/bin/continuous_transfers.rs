use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
use tokio::time::sleep;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

const TREE_DEPTH: u8 = 16;
const DOMAIN: [u8; 32] = [1u8; 32];
const NF_KEY: [u8; 32] = [4u8; 32];

#[derive(Clone, Debug)]
struct ContinuousConfig {
    num_wallets: usize,
    initial_deposit: bool,
    per_tx_delay_ms: u64,
    cycle_delay_ms: u64,
    external_node_url: String,
    external_verifier_url: String,
    max_concurrent_proofs: usize,
}

impl ContinuousConfig {
    fn from_env() -> Result<Self> {
        let num_wallets = std::env::var("NUM_WALLETS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);

        let initial_deposit = std::env::var("INITIAL_DEPOSIT")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);

        let per_tx_delay_ms = std::env::var("PER_TX_DELAY_MS")
            .or_else(|_| std::env::var("E2E_TRANSFER_DELAY_MS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        let cycle_delay_ms = std::env::var("CYCLE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000);

        let external_node_url = std::env::var("E2E_ROLLUP_EXTERNAL_NODE_URL")
            .unwrap_or_else(|_| "http://localhost:12346".to_string());
        let external_verifier_url = std::env::var("E2E_ROLLUP_EXTERNAL_VERIFIER_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());

        let max_concurrent_proofs = std::env::var("MAX_CONCURRENT_PROOFS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(num_cpus::get);

        Ok(Self {
            num_wallets,
            initial_deposit,
            per_tx_delay_ms,
            cycle_delay_ms,
            external_node_url,
            external_verifier_url,
            max_concurrent_proofs,
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

#[derive(Clone, Debug)]
struct CycleSummary {
    num_transfers: usize,
    num_included: usize,
    batches: BTreeMap<u64, usize>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let config = ContinuousConfig::from_env()?;

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

        eprintln!(
            "[setup] wallet {} address={} starting_nonce={}",
            i, account.address, nonce
        );

        wallets.push(WalletState {
            account,
            nonce,
            value: 0,
            rho: [0u8; 32],
            recipient: [0u8; 32],
        });
    }

    // Optional initial deposits to create notes for each wallet
    if config.initial_deposit {
        eprintln!(
            "\n[setup] Performing initial deposits for {} wallets...",
            wallets.len()
        );
        perform_initial_deposits(
            &client,
            &http,
            &mut wallets,
            &chain_hash,
            config.per_tx_delay_ms,
        )
        .await?;
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

    let mut shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        cycle_idx += 1;
        eprintln!("\n[cycle] ===== Transfer cycle {} =====", cycle_idx);

        let summary = tokio::select! {
            _ = &mut shutdown => {
                log_final_summary(cycle_idx - 1, total_transfers, total_included, &total_batches);
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

        eprintln!(
            "[cycle] Inclusion: {}/{} transfers included",
            summary.num_included, summary.num_transfers
        );
        for (batch, count) in &summary.batches {
            eprintln!("[cycle]   Batch {}: {} txs", batch, count);
        }

        total_transfers += summary.num_transfers;
        total_included += summary.num_included;
        for (batch, count) in summary.batches {
            *total_batches.entry(batch).or_insert(0) += count;
        }

        eprintln!(
            "[summary] so far: cycles={} transfers={} included={}",
            cycle_idx, total_transfers, total_included
        );

        eprintln!(
            "[cycle] Sleeping {} ms before next cycle...",
            config.cycle_delay_ms
        );

        tokio::select! {
            _ = &mut shutdown => {
                log_final_summary(cycle_idx, total_transfers, total_included, &total_batches);
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
) {
    eprintln!("\n[final-summary] =================================");
    eprintln!(
        "[final-summary] cycles={} total_transfers={} total_included={}",
        cycles, total_transfers, total_included
    );
    eprintln!("[final-summary] Batch distribution across all cycles:");
    for (batch, count) in total_batches {
        eprintln!(
            "[final-summary]   Batch {:3}: {:3} txs",
            batch, count
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
) -> Result<()> {
    for (i, wallet) in wallets.iter_mut().enumerate() {
        let amount: u128 = 100;
        let rho: Hash32 = rand::random();
        let recipient: Hash32 = rand::random();

        let call =
            RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Deposit {
                amount,
                rho,
                recipient,
                gas: None,
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
        eprintln!(
            "[deposit] wallet={} nonce={} amount={} url={}",
            i, wallet.nonce, amount, url
        );

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
    // Fetch tree state and all notes
    let state: TreeState = client
        .query_rest_endpoint("/modules/midnight-privacy/tree/state")
        .await
        .context("Failed to query tree state")?;

    let mut all_notes = Vec::new();
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
        let len = batch_resp.notes.len();
        all_notes.extend(batch_resp.notes);
        if len < batch_size {
            break;
        }
        offset += batch_size;
    }

    eprintln!(
        "[cycle] tree: next_position={} notes_count={}",
        state.next_position,
        all_notes.len()
    );

    // Rebuild Merkle tree and map commitments to positions
    let mut sorted_notes = all_notes.clone();
    sorted_notes.sort_by_key(|n| n.position);

    let mut mt = MerkleTree::new(TREE_DEPTH);
    for n in sorted_notes.iter() {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            mt.set_leaf(n.position as usize, cm);
        }
    }
    let rebuilt_root = mt.root();
    if rebuilt_root.as_slice() != state.root.as_slice() {
        bail!(
            "Rebuilt tree root mismatch: rebuilt={} vs state={}",
            hex::encode(rebuilt_root),
            hex::encode(&state.root)
        );
    }

    let mut pos_by_cm: HashMap<[u8; 32], u64> = HashMap::new();
    for n in &all_notes {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            pos_by_cm.insert(cm, n.position);
        }
    }

    let mut anchor_root = [0u8; 32];
    anchor_root.copy_from_slice(&state.root);

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
        if let Some(&position) = pos_by_cm.get(&cm) {
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
        });
    }

    eprintln!(
        "[cycle] Building proofs for {} wallets (max_concurrent_proofs={})",
        inputs.len(),
        config.max_concurrent_proofs
    );

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

    eprintln!(
        "[cycle] Generated {} transfer proofs (one per wallet)",
        proofs.len()
    );

    // Build and send transfer transactions to verifier (deferred submission)
    let mut transfer_txs_b64: Vec<(usize, String)> = Vec::with_capacity(proofs.len());
    let mut transfer_hashes: Vec<String> = Vec::with_capacity(proofs.len());
    for (i, (wallet_idx, proof_bytes, out_rho, out_recipient)) in proofs.into_iter().enumerate() {
        let wallet = &mut wallets[wallet_idx];

        let nf = nullifier(&DOMAIN, &NF_KEY, &wallet.rho);
        let call =
            RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Transfer {
                proof: <sov_modules_api::SafeVec<u8, 5_000_000>>::try_from(proof_bytes)
                    .map_err(|_| anyhow!("Proof too large for SafeVec"))?,
                anchor_root,
                nullifier: nf,
                gas: None,
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
            .context("Transfer tx signature verify failed")?;

        let tx_bytes = borsh::to_vec(&tx)?;
        let tx_hash = tx.hash().to_string();
        let tx_b64 = BASE64_STANDARD.encode(&tx_bytes);

        eprintln!(
            "  [transfer] wallet={} idx_in_cycle={} nonce={} tx={} nullifier={}",
            wallet_idx,
            i + 1,
            wallet.nonce,
            tx_hash,
            hex::encode(&nf[..8])
        );

        transfer_hashes.push(tx_hash);
        wallet.nonce += 1;
        wallet.rho = out_rho;
        wallet.recipient = out_recipient;

        transfer_txs_b64.push((wallet_idx, tx_b64));
    }

    eprintln!(
        "[cycle] Submitting {} transfers to verifier with deferred submission...",
        transfer_txs_b64.len()
    );

    for (idx, (_wallet_idx, body_b64)) in transfer_txs_b64.into_iter().enumerate() {
        let display_idx = idx + 1;
        let resp = http
            .post(format!(
                "{}/midnight-privacy",
                config.external_verifier_url
            ))
            .json(&json!({ "body": body_b64 }))
            .send()
            .await
            .with_context(|| format!("transfer #{} request failed", display_idx))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "transfer #{} verifier returned status {}: {}",
                display_idx,
                status, body
            );
        }

        if config.per_tx_delay_ms > 0 {
            sleep(Duration::from_millis(config.per_tx_delay_ms)).await;
        }
    }

    eprintln!("[cycle] All transfers submitted to verifier; flushing to sequencer...");
    let resp = http
        .post(format!(
            "{}/midnight-privacy/flush",
            config.external_verifier_url
        ))
        .send()
        .await
        .context("flush request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "flush endpoint returned status {}: {}",
            status, body
        );
    }
    eprintln!("[cycle] Flush complete.");

    // After flush, verify inclusion and collect per-batch statistics
    let mut batches: BTreeMap<u64, usize> = BTreeMap::new();
    let mut num_included = 0usize;
    let num_transfers = transfer_hashes.len();

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
                    *batches.entry(ltx.batch_number).or_insert(0) += 1;
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

    Ok(CycleSummary {
        num_transfers,
        num_included,
        batches,
    })
}
