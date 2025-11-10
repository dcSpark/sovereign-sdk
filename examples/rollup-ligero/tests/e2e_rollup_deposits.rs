use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use demo_stf::runtime::{Runtime, RuntimeCall};
use midnight_privacy::{CallMessage as MidnightCallMessage, Hash32};
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_api_spec::types as api_types;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_api::gas::UnlimitedGasMeter;
use sov_modules_api::{PrivateKey, PublicKey};
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_node_client::NodeClient;
use sov_test_utils::default_test_signed_transaction;
use tokio::time::sleep;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

// Proof verifier service (runs alongside the node)
use sov_proof_verifier_service::{create_router, AppState, ServiceConfig, RollupSpec};

// Match the spec used by the demo rollup binary
type DemoRollupSpec = <sov_rollup_ligero::MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

// We no longer rely on the compiled CHAIN_HASH; fetch from /rollup/schema instead.

fn find_binary() -> Result<String> {
    // Cargo sets this for integration tests of the same package
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_sov-rollup-ligero") {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_sov_rollup_ligero") {
        return Ok(p);
    }

    // Fallback: compute from target dir
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let target_dir = std::env::var("CARGO_TARGET_DIR").ok().unwrap_or_else(|| {
        // workspace target = two levels up from this crate dir
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.parent().unwrap().parent().unwrap().join("target").to_string_lossy().to_string()
    });
    let candidate = std::path::Path::new(&target_dir).join(&profile).join("sov-rollup-ligero");
    anyhow::ensure!(candidate.exists(), "sov-rollup-ligero binary not found; run `cargo build -p sov-rollup-ligero`.");
    Ok(candidate.to_string_lossy().to_string())
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

async fn wait_for_ready(client: &NodeClient, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for rollup to be ready");
        }
        if client.client.is_ready().await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")] // real E2E: starts a real rollup process
async fn e2e_deposits_demo_runtime() -> Result<()> {
    // Arrange: prepare isolated config and data dir
    let crate_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let base_cfg_path = crate_dir.join("rollup_config.toml");
    let base_cfg = std::fs::read_to_string(&base_cfg_path)
        .with_context(|| format!("Failed to read base config at {}", base_cfg_path.display()))?;

    // Reserve a free TCP port for the HTTP API
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let http_port = listener.local_addr()?.port();
    drop(listener);

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("demo_data");
    std::fs::create_dir_all(&data_dir)?;
    let new_cfg = make_temp_config(&base_cfg, &data_dir, http_port);
    let cfg_path = temp.path().join("rollup_config.toml");
    std::fs::write(&cfg_path, new_cfg)?;

    // Locate the binary
    let bin_path = find_binary()?;

    // Spawn the rollup
    let mut child = Command::new(&bin_path)
        .current_dir(&crate_dir)
        .arg("--rollup-config-path")
        .arg(cfg_path.as_os_str())
        .arg("--prometheus-exporter-bind")
        .arg("127.0.0.1:0")
        .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sov-rollup-ligero")?;

    // Ensure child is killed on panic/return
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }
    // Stream logs to help debugging readiness issues
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

    let mut guard = ChildGuard(child);

    let api_url = format!("http://127.0.0.1:{}", http_port);
    let client = NodeClient::new_unchecked(&api_url);
    // With faster DA config above this should be quick; allow up to 90s for first run
    wait_for_ready(&client, Duration::from_secs(90)).await?;

    // Fetch the authoritative chain_hash from the node's schema endpoint (used for verifier + signing)
    #[derive(serde::Deserialize)]
    struct SchemaResp { chain_hash: String }
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

    // Start the proof-verifier service bound to the same DA DB and node RPC
    // Create a temporary signing key file for the service
    let verifier_url: String = {
        use sov_rollup_interface::crypto::PrivateKey as _;

        let sk: <<RollupSpec as sov_modules_api::Spec>::CryptoSpec as sov_rollup_interface::zk::CryptoSpec>::PrivateKey =
            <<RollupSpec as sov_modules_api::Spec>::CryptoSpec as sov_rollup_interface::zk::CryptoSpec>::PrivateKey::generate();
        let pk = sk.pub_key();
        let addr: <RollupSpec as sov_modules_api::Spec>::Address = pk.credential_id().into();

        let tmpkey = tempfile::NamedTempFile::new().context("Failed to create temp key file")?;
        std::fs::write(
            tmpkey.path(),
            serde_json::to_string(&serde_json::json!({
                "private_key": sk,
                "address": addr,
            }))?,
        )?;

        // Build DA connection string matching our temp config
        let da_connection_string = format!("sqlite://{}/da.sqlite?mode=rwc", data_dir.display());
        let verifier_cfg = ServiceConfig {
            node_rpc_url: api_url.clone(),
            signing_key_path: tmpkey.path().to_string_lossy().to_string(),
            value_setter_method_id: None,
            midnight_method_id: None, // deposits don't require a Ligero method id
            max_concurrent_verifications: 4,
            chain_id: 1,
            chain_hash: Some(chain_hash),
            da_connection_string: da_connection_string.clone(),
        };
        let state = AppState::new(verifier_cfg).await.context("Failed to create AppState")?;
        let app = create_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let service_addr = listener.local_addr()?;
        let verifier_url = format!("http://{}", service_addr);
        tokio::spawn(async move {
            axum::serve(
                listener,
                axum::ServiceExt::<axum::extract::Request>::into_make_service(app),
            )
            .await
            .expect("Failed to serve proof verifier service");
        });
        // Keep temp key alive
        std::mem::forget(tmpkey);

        // We will submit via this verifier URL
        // Submit small health probe (optional).
        let hc = reqwest::Client::new();
        let _ = hc.get(format!("{}/health", verifier_url)).send().await;

        // Store verifier_url in a local var for later use
        // Shadowing api_url is avoided; we keep both
        verifier_url
    };

    // Load funded private key that matches genesis
    let key_path = crate_dir
        .parent()
        .unwrap() // examples/
        .join("test-data/keys/tx_signer_private_key.json");
    let key_data: PrivateKeyAndAddress<DemoRollupSpec> =
        serde_json::from_str(&std::fs::read_to_string(&key_path)?).with_context(|| {
            format!("Failed to read/parse key file at {}", key_path.display())
        })?;

    // Fetch current nonce from node
    let pub_key = key_data.private_key.pub_key();
    let starting_nonce = client
        .get_nonce_for_public_key::<DemoRollupSpec>(&pub_key)
        .await
        .context("Failed to fetch nonce")?;

    // Capture initial module state for robust delta checks
    #[derive(serde::Deserialize, Clone, Debug)]
    struct TreeState { root: Vec<u8>, next_position: u64 }
    #[derive(serde::Deserialize, Clone, Copy, Debug, Default)]
    struct Stats { deposit_count: u64 }
    let initial_tree: TreeState = client
        .query_rest_endpoint("/modules/midnight-privacy/tree/state")
        .await
        .context("Failed to query initial midnight-privacy tree state")?;
    let initial_stats: Stats = client
        .query_rest_endpoint("/modules/midnight-privacy/stats")
        .await
        .unwrap_or_default();

    // Number of deposits (default 10)
    let num_deposits: usize = std::env::var("NUM_DEPOSITS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    #[derive(serde::Serialize)]
    struct Body<'a> { body: &'a str }
    let http = reqwest::Client::new();
    let mut tx_hashes_hex: Vec<String> = Vec::with_capacity(num_deposits);
    let mut sent_meta: Vec<(String, u128, String, String)> = Vec::with_capacity(num_deposits);

    for i in 0..num_deposits {
        // Build a midnight deposit tx for demo runtime
        let amount: u128 = 100;
        let rho: Hash32 = rand::random();
        let recipient: Hash32 = rand::random();
        let call = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(MidnightCallMessage::Deposit {
            amount,
            rho,
            recipient,
            view_fvks: None,
            gas: None,
        });

        let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> = default_test_signed_transaction(
            &key_data.private_key,
            &call,
            starting_nonce + i as u64,
            &chain_hash,
        );
        // Local sanity check: signature should verify
        let mut meter = UnlimitedGasMeter::<DemoRollupSpec>::default();
        tx.verify(&chain_hash, &mut meter)
            .context("Local signature verification failed")?;
        let tx_bytes = borsh::to_vec(&tx)?;

        // Submit to the verifier service (preferred) with fallback to sequencer direct
        let tx_b64 = BASE64_STANDARD.encode(&tx_bytes);
        let service_response = http
            .post(format!("{}/midnight-privacy", verifier_url))
            .json(&Body { body: &tx_b64 })
            .send()
            .await;
        if let Ok(resp) = service_response {
            if !resp.status().is_success() {
                let _ = client
                    .send_transactions_to_sequencer(vec![tx_bytes.clone()], true)
                    .await;
            }
        } else {
            let _ = client
                .send_transactions_to_sequencer(vec![tx_bytes.clone()], true)
                .await;
        }

        let tx_hash_str = tx.hash().to_string();
        eprintln!(
            "[debug] sending deposit tx={} amount={} rho={} recipient={}",
            tx_hash_str,
            amount,
            hex::encode(&rho[..8]),
            hex::encode(&recipient[..8])
        );
        sent_meta.push((
            tx_hash_str.clone(),
            amount,
            hex::encode(&rho[..8]),
            hex::encode(&recipient[..8]),
        ));
        tx_hashes_hex.push(tx_hash_str);
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
    for hash_hex in &tx_hashes_hex {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match client
                .query_rest_endpoint::<api_types::LedgerTx>(&format!("/ledger/txs/{}?children=1", hash_hex))
                .await
            {
                Ok(ltx) => {
                    // Found in ledger: assert receipt is successful
                    anyhow::ensure!(
                        ltx.receipt.result == api_types::TxReceiptResult::Successful,
                        "Tx {} included but not successful: {:?}",
                        hash_hex,
                        ltx.receipt
                    );
                    // Ensure it is a MidnightPrivacy deposit and count it
                    let has_pool_deposit = ltx
                        .events
                        .iter()
                        .any(|ev| ev.key == "ValueMidnightPrivacy/PoolDeposit");
                    if !has_pool_deposit {
                        let keys: Vec<String> = ltx
                            .events
                            .iter()
                            .map(|ev| format!("{}", ev.key))
                            .collect();
                        let raw_json = client
                            .http_get(&format!("/ledger/txs/{}?children=1", hash_hex))
                            .await
                            .unwrap_or_else(|e| format!("<failed to fetch ledger json: {e}>"));
                        if let Some((_, amt, rho_hex, rec_hex)) =
                            sent_meta.iter().find(|(h, ..)| h == hash_hex)
                        {
                            eprintln!(
                                "[debug] expected deposit: amount={} rho={} recipient={}",
                                amt, rho_hex, rec_hex
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
                    // Positive confirmation log for successful inclusion
                    eprintln!(
                        "[ok] included deposit tx={} batch_number={} tx_number={} events={}",
                        hash_hex,
                        ltx.batch_number,
                        ltx.number,
                        ltx.events.len()
                    );
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
    eprintln!(
        "[ok] all deposits included: {}/{}",
        included_deposits,
        num_deposits
    );

    // Verify the module state advanced (at least one note)
    // Poll tree state until epilogue flushes queued outputs
    let start = std::time::Instant::now();
    let target = Duration::from_secs(10);
    let mut observed: Option<TreeState> = None;
    loop {
        let state: TreeState = client
            .query_rest_endpoint("/modules/midnight-privacy/tree/state")
            .await
            .context("Failed to query midnight-privacy tree state")?;
        anyhow::ensure!(state.root.len() == 32, "Invalid root length");
        if state.next_position >= initial_tree.next_position + num_deposits as u64 { observed = Some(state); break; }
        if start.elapsed() > target { observed = Some(state); break; }
        sleep(Duration::from_millis(100)).await;
    }
    let state = observed.unwrap();
    anyhow::ensure!(
        state.next_position >= initial_tree.next_position + num_deposits as u64,
        "Tree did not advance; expected delta >= {} (initial: {}, final: {})",
        num_deposits,
        initial_tree.next_position,
        state.next_position
    );
    eprintln!(
        "[ok] tree advanced by >= {} (initial_next_position={}, final_next_position={})",
        num_deposits,
        initial_tree.next_position,
        state.next_position
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
                if final_stats.deposit_count >= stats_target { break; }
            }
            Err(_) => { /* ignore transient errors and keep polling */ }
        }
        if stats_start.elapsed() > stats_timeout { break; }
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
        stats_target,
        initial_stats.deposit_count,
        final_stats.deposit_count
    );

    // Cleanup process
    let _ = guard.0.kill();
    let _ = guard.0.wait();

    Ok(())
}
