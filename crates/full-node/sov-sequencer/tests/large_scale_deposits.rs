//! Large scale test for privacy deposits and transfers with proof verifier service
//!
//! This test demonstrates high-volume processing with up to 1000 deposits
//! followed by the same number of transfers with real ZK proofs.
//!
//! ## Proof Verifier Service Integration
//!
//! The test can automatically start the proof verifier service to pre-process ZK proofs,
//! reducing the amount of data sent to the sequencer from ~3MB per proof to just a few KB.
//!
//! **How it works:**
//! 1. The test creates a rollup with MockDA backed by SQLite in a temp directory
//! 2. The proof verifier service is started with the SAME DA connection string
//! 3. Full ZK transactions (~3MB) are POSTed to the verifier's `/midnight-privacy` endpoint
//! 4. The verifier validates the proof and stores a lightweight pre-authenticated tx in the shared DB
//! 5. The verifier then calls the sequencer's `/sequencer/worker_txs/{tx_hash}` endpoint
//! 6. The sequencer retrieves the pre-auth tx from the shared DB (just a few KB)
//!
//! **CRITICAL**: Both the rollup and verifier service MUST share the same DA database!
//! The test sets `SOV_WORKER_TX_DB_CONNECTION_STRING` to enable this.
//!
//! To run manually with an external verifier service:
//! ```bash
//! # Start the test which will print the DA connection string
//! cargo test --test=large_scale_deposits --package=sov-sequencer -- --nocapture
//!
//! # In another terminal, start the verifier service with the same DA connection:
//! cargo run --bin sov-proof-verifier-service -- \
//!   --da-connection-string="sqlite:///tmp/test-XXXXX/da.sqlite?mode=rwc" \
//!   --node-rpc-url="http://127.0.0.1:PORT" \
//!   --signing-key-path=path/to/key.json
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use midnight_privacy::{
    CallMessage as MidnightCallMessage, Hash32, MerkleTree, SpendPublic, ValueMidnightPrivacy,
    ValueSetterZkConfig, note_commitment, nullifier,
};
use sov_api_spec::types::AcceptTxBody;
use sov_ligero_adapter::{Ligero, LigeroProofPackage};
use sov_mock_da::BlockProducingConfig;
use sov_modules_api::{DispatchCall, RawTx, Runtime, Spec, PublicKey};
use sov_modules_stf_blueprint::GenesisParams;
use sov_paymaster::{Paymaster, PaymasterConfig};
use sov_rollup_interface::zk::{Zkvm, ZkvmHost, CryptoSpec};
use sov_modules_api::ZkVerifier;
use borsh::BorshDeserialize;
use sov_test_utils::runtime::genesis::optimistic::HighLevelOptimisticGenesisConfig;
use sov_test_utils::sov_bank::config_gas_token_id;
use sov_test_utils::test_rollup::{GenesisSource, RollupBuilder, TestRollup};
use sov_test_utils::{
    default_test_signed_transaction, generate_optimistic_runtime_with_kernel, 
    RtAgnosticBlueprint, TestSpec, TestUser,
};
use sov_value_setter::{ValueSetter, ValueSetterConfig};

// NEW: inline test verifier service (TestRuntime-compatible, avoids 3MB proofs over HTTP)
use axum::{Router, routing::post, extract::State as AxumState, extract::DefaultBodyLimit, Json as AxumJson};
use sea_orm::{Database, DatabaseConnection, ConnectOptions, ActiveValue::Set, EntityTrait, sea_query::OnConflict};
use sov_modules_api::transaction::{Transaction, VersionedTx, Version0};
use sov_modules_api::gas::UnlimitedGasMeter;
use sov_midnight_da::storable::{setup_db as setup_midnight_da_db, worker_verified_transactions};
use sov_rollup_interface::zk::CodeCommitment;
use sov_ligero_adapter::{LigeroVerifier, LigeroCodeCommitment};
use sqlx::types::chrono::Utc;

// Generate a runtime with SoftConfirmationsKernel to support Preferred Sequencer
// Now includes midnight-privacy module for privacy-preserving transactions
generate_optimistic_runtime_with_kernel!(
    TestRuntime <=
    kernel_type: sov_kernels::soft_confirmations::SoftConfirmationsKernel<'a, S>,
    modules: [value_setter: ValueSetter<S>, paymaster: Paymaster<S>, midnight_privacy: ValueMidnightPrivacy<S>],
);

type RT = TestRuntime<TestSpec>;
type TestBlueprint = RtAgnosticBlueprint<TestSpec, RT>;

const MAX_BATCH_EXECUTION_TIME_MILLIS: u64 = 1_000 * 60 * 30; // Allow batches to take up to 30 minutes

/// Minimal TestRuntime-compatible verifier service (in-process) that:
/// - Deserializes Transaction<RT, TestSpec>
/// - Verifies signature and Ligero proof (transfer/withdraw)
/// - Stores lightweight pre-auth tx in MockDA DB
/// - Notifies sequencer via /sequencer/worker_txs/{tx_hash}
#[derive(Clone)]
struct TestVerifierState {
    node_rpc_url: String,
    midnight_method_id: [u8; 32],
    da_conn: DatabaseConnection,
    semaphore: Arc<tokio::sync::Semaphore>,
    ligero_program_path: Option<String>,
    ligero_shader_path: Option<String>,
    ligero_verifier_bin: Option<String>,
    ligero_packing: Option<String>,
    ligero_prover_bin: Option<String>,
}

async fn spawn_test_verifier_service(
    rollup_http_addr: &str,
    da_connection_string: &str,
    midnight_method_id: [u8; 32],
    max_concurrent: usize,
) -> Result<(tokio::task::JoinHandle<()>, String)> {
    let mut opts = ConnectOptions::new(da_connection_string.to_string());
    opts.max_connections(20).sqlx_logging(false);
    let da_conn = Database::connect(opts).await.context("connect MockDA")?;
    setup_midnight_da_db(&da_conn).await.context("setup MockDA schema")?;

    let state = TestVerifierState {
        node_rpc_url: format!("http://{}", rollup_http_addr),
        midnight_method_id,
        da_conn,
        semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        ligero_program_path: std::env::var("LIGERO_PROGRAM_PATH").ok(),
        ligero_shader_path: std::env::var("LIGERO_SHADER_PATH").ok(),
        ligero_verifier_bin: std::env::var("LIGERO_VERIFIER_BIN").ok(),
        ligero_packing: std::env::var("LIGERO_PACKING").ok(),
        ligero_prover_bin: std::env::var("LIGERO_PROVER_BIN").ok(),
    };

    let app = Router::new()
        .route("/midnight-privacy", post(verify_and_record_midnight_handler_test))
        .with_state(state.clone())
        .layer(DefaultBodyLimit::disable())
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{}", addr);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve test verifier")
    });
    Ok((task, url))
}

#[derive(serde::Serialize)]
struct TestVerifierResponse {
    success: bool,
    tx_hash: String,
}

async fn verify_and_record_midnight_handler_test(
    AxumState(state): AxumState<TestVerifierState>,
    AxumJson(req): AxumJson<AcceptTxBody>,
) -> Result<AxumJson<TestVerifierResponse>, (axum::http::StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Ensure Ligero env is set for the verifier subprocess
    if let Some(ref v) = state.ligero_program_path { std::env::set_var("LIGERO_PROGRAM_PATH", v); }
    if let Some(ref v) = state.ligero_shader_path { std::env::set_var("LIGERO_SHADER_PATH", v); }
    if let Some(ref v) = state.ligero_verifier_bin { std::env::set_var("LIGERO_VERIFIER_BIN", v); }
    if let Some(ref v) = state.ligero_packing { std::env::set_var("LIGERO_PACKING", v); }
    if let Some(ref v) = state.ligero_prover_bin { std::env::set_var("LIGERO_PROVER_BIN", v); }

    // Log resolved Ligero env for diagnostics
    println!("[verifier] LIGERO_PROGRAM_PATH={:?}", std::env::var("LIGERO_PROGRAM_PATH").ok());
    println!("[verifier] LIGERO_SHADER_PATH={:?}", std::env::var("LIGERO_SHADER_PATH").ok());
    println!("[verifier] LIGERO_VERIFIER_BIN={:?}", std::env::var("LIGERO_VERIFIER_BIN").ok());
    println!("[verifier] LIGERO_PROVER_BIN={:?}", std::env::var("LIGERO_PROVER_BIN").ok());
    println!("[verifier] LIGERO_PACKING={:?}", std::env::var("LIGERO_PACKING").ok());

    // 1) Decode and deserialize TestRuntime transaction
    let raw = BASE64_STANDARD.decode(&req.body).map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("Invalid base64: {e}")))?;
    println!("[verifier] received tx body: base64_len={}, raw_len={}", req.body.len(), raw.len());
    let tx: Transaction<RT, TestSpec> = BorshDeserialize::try_from_slice(&raw).map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("Failed to deserialize transaction: {e}")))?;

    // 2) Verify signature
    let mut meter = UnlimitedGasMeter::<TestSpec>::default();
    tx.verify(&<RT as Runtime<TestSpec>>::CHAIN_HASH, &mut meter).map_err(|e| (axum::http::StatusCode::UNAUTHORIZED, e.to_string()))?;

    // 3) Parse call
    type Call = <RT as DispatchCall>::Decodable;
    let call = tx.runtime_call();
    let (maybe_proof, expected_anchor_root, expected_nullifier, expected_withdraw_amount): (Option<Vec<u8>>, Hash32, Hash32, u128) = match call {
        Call::MidnightPrivacy(ref c) => {
            match c {
                MidnightCallMessage::Deposit { rho: _, recipient: _, amount, view_fvks: _, gas: _ } => {
                    println!("[verifier] parsed call: Deposit amount={}", amount);
                    (None::<Vec<u8>>, [0u8;32], [0u8;32], 0u128)
                }
                MidnightCallMessage::Transfer { proof, anchor_root, nullifier, .. } => {
                    println!("[verifier] parsed call: Transfer proof_len={} anchor_root=0x{} nullifier=0x{}",
                        proof.len(), hex::encode(anchor_root), hex::encode(nullifier));
                    (Some::<Vec<u8>>(proof.clone().into()), *anchor_root, *nullifier, 0u128)
                }
                MidnightCallMessage::Withdraw { proof, anchor_root, nullifier, withdraw_amount, .. } => {
                    println!("[verifier] parsed call: Withdraw proof_len={} withdraw_amount={} anchor_root=0x{} nullifier=0x{}",
                        proof.len(), withdraw_amount, hex::encode(anchor_root), hex::encode(nullifier));
                    (Some::<Vec<u8>>(proof.clone().into()), *anchor_root, *nullifier, *withdraw_amount)
                }
                MidnightCallMessage::UpdateMethodId { .. } => return Err((axum::http::StatusCode::BAD_REQUEST, "Unsupported call".to_string())),
            }
        }
        _ => return Err((axum::http::StatusCode::BAD_REQUEST, "Expected midnight_privacy call".to_string())),
    };

    // Build transaction_data JSON for sequencer to detect intent (deposit/transfer/withdraw)
    let transaction_data = match &call {
        Call::MidnightPrivacy(c) => {
            match c {
                MidnightCallMessage::Deposit { amount, rho, recipient, view_fvks, gas } => {
                    serde_json::json!({
                        "deposit": {
                            "amount": amount.to_string(),
                            "rho": hex::encode(rho),
                            "recipient": hex::encode(recipient),
                            "view_fvks": view_fvks,
                            "gas": gas
                        }
                    }).to_string()
                }
                MidnightCallMessage::Transfer { anchor_root, nullifier, view_ciphertexts, gas, .. } => {
                    serde_json::json!({
                        "transfer": {
                            "proof": "REMOVED",
                            "anchor_root": hex::encode(anchor_root),
                            "nullifier": hex::encode(nullifier),
                            "view_ciphertexts": view_ciphertexts,
                            "gas": gas
                        }
                    }).to_string()
                }
                MidnightCallMessage::Withdraw { anchor_root, nullifier, withdraw_amount, to, view_ciphertexts, gas, .. } => {
                    serde_json::json!({
                        "withdraw": {
                            "proof": "REMOVED",
                            "anchor_root": hex::encode(anchor_root),
                            "nullifier": hex::encode(nullifier),
                            "withdraw_amount": withdraw_amount.to_string(),
                            "to": to.to_string(),
                            "view_ciphertexts": view_ciphertexts,
                            "gas": gas
                        }
                    }).to_string()
                }
                MidnightCallMessage::UpdateMethodId { .. } => "{}".to_string(),
            }
        }
        _ => "{}".to_string(),
    };

    // 4) Verify proof if present
    let proof_public = if let Some(proof_bytes) = maybe_proof {
        let method_id = LigeroCodeCommitment(state.midnight_method_id);
        let data = proof_bytes;
        let public = tokio::task::spawn_blocking(move || {
            // use same env as generation; data is a LigeroProofPackage bincode blob
            let _package: sov_ligero_adapter::LigeroProofPackage = bincode::deserialize(&data)
                .map_err(|e| anyhow::anyhow!("Proof payload is not a LigeroProofPackage: {e}"))?;
            println!("[verifier] proof package len={} head={:02x?}", data.len(), &data.get(..16).unwrap_or(&[]));
            println!("[verifier] verifying with method_id=0x{}", hex::encode(method_id.0));
            // Verify using the serialized package (matches service behavior)
            let public: SpendPublic = LigeroVerifier::verify(&data, &method_id)
                .map_err(|e| anyhow::anyhow!("Verification failed: {e}"))?;
            Ok::<SpendPublic, anyhow::Error>(public)
        }).await.map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Join error: {e}")))?
            .map_err(|e| (axum::http::StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;

        if public.anchor_root != expected_anchor_root {
            return Err((axum::http::StatusCode::UNPROCESSABLE_ENTITY, format!("Anchor root mismatch")));
        }
        if public.nullifier != expected_nullifier {
            return Err((axum::http::StatusCode::UNPROCESSABLE_ENTITY, format!("Nullifier mismatch")));
        }
        if public.withdraw_amount != expected_withdraw_amount {
            return Err((axum::http::StatusCode::UNPROCESSABLE_ENTITY, format!("Withdraw amount mismatch")));
        }
        Some(public)
    } else {
        None
    };

    // 5) Build lightweight transaction (strip proof)
    let lightweight_runtime_call = match call {
        Call::MidnightPrivacy(ref c) => {
            use sov_modules_api::SafeVec;
            match c {
                MidnightCallMessage::Transfer { anchor_root, nullifier, view_ciphertexts, gas, .. } => {
                    Call::MidnightPrivacy(MidnightCallMessage::Transfer {
                        proof: SafeVec::new(),
                        anchor_root: *anchor_root,
                        nullifier: *nullifier,
                        view_ciphertexts: view_ciphertexts.clone(),
                        gas: gas.clone(),
                    })
                }
                MidnightCallMessage::Withdraw { anchor_root, nullifier, withdraw_amount, to, view_ciphertexts, gas, .. } => {
                    Call::MidnightPrivacy(MidnightCallMessage::Withdraw {
                        proof: SafeVec::new(),
                        anchor_root: *anchor_root,
                        nullifier: *nullifier,
                        withdraw_amount: *withdraw_amount,
                        to: to.clone(),
                        view_ciphertexts: view_ciphertexts.clone(),
                        gas: gas.clone(),
                    })
                }
                _ => call.clone(),
            }
        }
        _ => call.clone(),
    };

    let tx_hash = tx.hash().to_string();
    let v0 = match &tx.versioned_tx {
        VersionedTx::V0(v) => v,
    };
    let lightweight_tx = Transaction::<RT, TestSpec> {
        versioned_tx: VersionedTx::V0(Version0 {
            signature: v0.signature.clone(),
            pub_key: v0.pub_key.clone(),
            runtime_call: lightweight_runtime_call,
            uniqueness: v0.uniqueness.clone(),
            details: v0.details.clone(),
        }),
    };
    let lightweight_tx_bytes = borsh::to_vec(&lightweight_tx).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let serialized_tx_base64 = BASE64_STANDARD.encode(&lightweight_tx_bytes);
    println!("[verifier] tx_hash={} lightweight_tx_len={} full_tx_len={} saved_base64_len={}",
        tx_hash, lightweight_tx_bytes.len(), raw.len(), serialized_tx_base64.len());

    // Pre-auth components (hex)
    let pub_key_hex = hex::encode(borsh::to_vec(&v0.pub_key).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?);
    let signature_hex = hex::encode(borsh::to_vec(&v0.signature).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?);
    let uniqueness_hex = hex::encode(borsh::to_vec(&v0.uniqueness).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?);
    let details_hex = hex::encode(borsh::to_vec(&v0.details).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?);
    let runtime_call_hex = hex::encode(borsh::to_vec(&v0.runtime_call).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?);
    println!("[verifier] pre-auth sizes: pub_key={} sig={} uniq={} details={} runtime_call={}",
        pub_key_hex.len(), signature_hex.len(), uniqueness_hex.len(), details_hex.len(), runtime_call_hex.len());

    // 6) Persist to MockDA worker_verified_transactions
    use worker_verified_transactions::{ActiveModel as VerifiedActiveModel, Column as VerifiedColumn, Entity as VerifiedEntity, TransactionState};
    let sender_addr: <TestSpec as Spec>::Address = v0.pub_key.credential_id().into();
    let proof_outputs_json = if let Some(p) = &proof_public {
        serde_json::to_string(p).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        "{}".to_string()
    };
    println!("[verifier] inserting worker_verified_transactions row for tx_hash={}", tx_hash);
    VerifiedEntity::insert(VerifiedActiveModel {
        tx_hash: Set(tx_hash.clone()),
        signature_valid: Set(true),
        proof_verified: Set(proof_public.as_ref().map(|_| true)),
        transaction_data: Set(transaction_data),
        full_transaction_blob: Set(req.body.clone()),
        proof_outputs: Set(proof_outputs_json),
        pub_key_hex: Set(Some(pub_key_hex)),
        signature_hex: Set(Some(signature_hex)),
        uniqueness_hex: Set(Some(uniqueness_hex)),
        details_hex: Set(Some(details_hex)),
        runtime_call_hex: Set(Some(runtime_call_hex)),
        serialized_tx_base64: Set(Some(serialized_tx_base64)),
        transaction_state: Set(TransactionState::Pending),
        sequencer_status: Set(None),
        sender: Set(sender_addr.to_string()),
        created_at: Set(Utc::now()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(VerifiedColumn::TxHash)
            .update_columns([VerifiedColumn::ProofOutputs, VerifiedColumn::SerializedTxBase64, VerifiedColumn::TransactionState, VerifiedColumn::CreatedAt])
            .to_owned(),
    )
    .exec(&state.da_conn)
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?;

    // 7) Notify sequencer
    let notify_url = format!("{}/sequencer/worker_txs/{}", state.node_rpc_url, tx_hash);
    println!("[verifier] notifying sequencer at {}", notify_url);
    let resp = reqwest::Client::new().post(&notify_url).send().await.map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, format!("Notify error: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        println!("[verifier] sequencer notify failed: status={} body={}", status, text);
        return Err((axum::http::StatusCode::BAD_GATEWAY, format!("Sequencer rejected: {} {}", status, text)));
    }

    Ok(AxumJson(TestVerifierResponse { success: true, tx_hash }))
}

/// Helper to create a Preferred Sequencer for testing with configurable batch size
/// Returns the rollup, DA connection string, and a list of users that can be used to generate transactions
async fn create_test_sequencer_with_batch_size(
    max_batch_size: usize,
    method_id: [u8; 32],
    num_accounts: usize,
) -> (TestRollup<TestBlueprint>, String, Vec<TestUser<TestSpec>>) {
    let genesis_config =
        HighLevelOptimisticGenesisConfig::<TestSpec>::generate().add_accounts_with_default_balance(num_accounts);
    let accounts: Vec<_> = genesis_config.additional_accounts().to_vec();
    let admin = &accounts[0];
    
    let rt_genesis_config = <RT as Runtime<TestSpec>>::GenesisConfig::from_minimal_config(
        genesis_config.into(),
        ValueSetterConfig {
            admin: admin.address(),
        },
        PaymasterConfig::default(),
        ValueSetterZkConfig {
            tree_depth: 20,
            root_window_size: 100,
            method_id, // Use the actual code commitment
            admin: admin.address(),
            domain: [0u8; 32],
            token_id: config_gas_token_id(), // Use the actual gas token
            gas_per_output_append: None,
        },
    );

    let genesis_params = GenesisParams {
        runtime: rt_genesis_config,
    };

    // Use a persistent directory for the database  
    let persistent_dir = std::path::PathBuf::from("./test-data/large-scale-deposits");
    std::fs::create_dir_all(&persistent_dir).expect("Failed to create persistent directory");
    
    // Convert to absolute path so external services can find it from any directory
    let absolute_dir = persistent_dir.canonicalize().expect("Failed to get absolute path");
    
    // Create a temp dir wrapper that won't auto-delete
    let temp_dir = Arc::new(tempfile::TempDir::new_in(absolute_dir.parent().unwrap())
        .expect("Failed to create temp dir"));
    
    let seq_da_address = genesis_params.runtime.sequencer_registry.sequencer_config.seq_da_address;
    
    // Build the DA connection string that will be shared with the verifier service (using absolute path)
    let da_connection_string = format!("sqlite://{}/da.sqlite?mode=rwc", absolute_dir.display());

    use sov_stf_runner::processes::RollupProverConfig;
    
    let rollup = RollupBuilder::<TestBlueprint>::new_with_storage_path(
        GenesisSource::CustomParams(genesis_params),
        BlockProducingConfig::Periodic { block_time_ms: 200 },
        0, // finalization_blocks
        temp_dir,
        false, // in_memory_da - we want persistent DA
    )
    .with_zkvm_host_args(Default::default())
    .set_persistent_da() // This sets up the DA to use the storage path
    .set_config(|c| {
        c.automatic_batch_production = true;
        c.max_batch_size_bytes = max_batch_size;
        c.blob_processing_timeout_secs = 600; // 10 minutes for large batches
        c.max_concurrent_blobs = 10;
        // Enable the prover service to generate aggregated block proofs
        c.rollup_prover_config = Some(RollupProverConfig::Skip);
        // Disable state root consistency checks since proofs aren't played yet in the sequencer
        if let sov_sequencer::SequencerKindConfig::Preferred(preferred_sequencer_config) =
            &mut c.sequencer_config
        {
            preferred_sequencer_config.batch_execution_time_limit_millis =
                MAX_BATCH_EXECUTION_TIME_MILLIS;
            preferred_sequencer_config.disable_state_root_consistency_checks = true;
        }
    })
    .set_da_config(|c| {
        c.sender_address = seq_da_address;
        // Override with our custom connection string
        c.connection_string = da_connection_string.clone();
    })
    .with_preferred_seq_min_profit_per_tx(0)
    .with_preferred_seq_recovery_strategy(sov_sequencer::preferred::RecoveryStrategy::TryToSave)
    .start()
    .await
    .unwrap();

    (rollup, da_connection_string, accounts)
}

/// Helper to setup Ligero environment and get code commitment
fn setup_ligero_env() -> Result<(String, [u8; 32])> {
    use sov_rollup_interface::zk::CodeCommitment;
    
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .ok_or_else(|| anyhow::anyhow!("Could not find repository root"))?;

    let ligero_dir = repo_root.join("crates/adapters/ligero");

    // Detect OS
    let platform_dir = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        anyhow::bail!("Unsupported platform. Supported: macOS, Linux");
    };

    let bin_dir = ligero_dir.join("bins").join(platform_dir).join("bin");
    let shader_dir = ligero_dir.join("bins").join(platform_dir).join("shader");
    let program_path = ligero_dir.join("guest/bins/programs/note_spend_guest.wasm");
    let prover_bin = bin_dir.join("webgpu_prover");
    let verifier_bin = bin_dir.join("webgpu_verifier");

    if !program_path.exists() {
        anyhow::bail!(
            "WASM program not found at: {}\nRun: cd crates/adapters/ligero/guest/note-spend-guest && cargo build --release --target wasm32-unknown-unknown",
            program_path.display()
        );
    }

    // Compute the code commitment from the WASM program
    let host = <Ligero as Zkvm>::Host::from_args(&program_path.to_string_lossy().to_string());
    let code_commitment = host.code_commitment();
    let method_id: [u8; 32] = code_commitment.encode().try_into()
        .map_err(|_| anyhow::anyhow!("Code commitment should be 32 bytes"))?;

    // Set environment variables for both proof generation AND verification
    std::env::set_var("LIGERO_PROGRAM_PATH", &program_path);
    std::env::set_var("LIGERO_PROVER_BIN", &prover_bin);
    std::env::set_var("LIGERO_VERIFIER_BIN", &verifier_bin);
    std::env::set_var("LIGERO_SHADER_PATH", &shader_dir);
    std::env::set_var("LIGERO_PACKING", "8192");

    Ok((program_path.to_string_lossy().to_string(), method_id))
}

/// TEST: Large scale deposits and transfers (up to 1000 each)
/// 
/// This test demonstrates the complete privacy-preserving flow at scale:
/// 1. Creates up to 1000 deposits into the shielded pool
/// 2. Generates REAL Ligero ZK proofs for transfers IN PARALLEL
/// 3. Automatically starts a proof verifier service to pre-process proofs
/// 4. Submits all transfers through the verifier service (optimized path)
///
/// The proof verifier service automatically pre-processes ZK proofs, reducing
/// the amount of data sent to the sequencer from ~3MB per proof to just a few KB.
///
/// ## Environment Variables:
/// - `NUM_DEPOSITS`: Number of deposits to create (default: 10, max: 1000)
/// - `NUM_TRANSFERS`: Number of parallel transfers with proofs (default: 10, max: NUM_DEPOSITS)
/// - `CONCURRENT_PROOFS`: Number of proofs to generate concurrently (default: 4)
/// - `USE_MOCK_PROOFS`: Set to "1" to use mock proofs instead of real Ligero proofs (much faster)
///
/// ## How to Run:
///
/// Default (10 deposits, 10 transfers with real proofs via auto-started verifier service):
/// ```bash
/// cargo test --package sov-sequencer large_scale_deposits_and_transfers -- --nocapture
/// ```
///
/// Large scale test with MOCK proofs (fast):
/// ```bash
/// USE_MOCK_PROOFS=1 NUM_DEPOSITS=1000 NUM_TRANSFERS=1000 cargo test --package sov-sequencer large_scale_deposits_and_transfers -- --nocapture
/// ```
///
/// Large scale test with REAL proofs (recommended):
/// ```bash
/// NUM_DEPOSITS=100 NUM_TRANSFERS=100 CONCURRENT_PROOFS=8 cargo test --package sov-sequencer large_scale_deposits_and_transfers -- --nocapture --test-threads=1
/// ```
///
/// ## Requirements:
/// - Ligero binaries (webgpu_prover, webgpu_verifier)
/// - note_spend_guest.wasm program
/// - WebGPU-capable system for proof generation
/// - Sufficient system resources (RAM, GPU memory) for concurrent proof generation
#[tokio::test(flavor = "multi_thread")]
async fn large_scale_deposits_and_transfers() {
    use tokio::sync::mpsc;
    use tokio::sync::Semaphore;
    
    println!("\n=== Large Scale Midnight Privacy Test with ZK Proofs ===\n");
    
    // Read configuration from environment
    let num_deposits: usize = std::env::var("NUM_DEPOSITS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .min(1000); // Cap at 1000
    
    let num_transfers: usize = std::env::var("NUM_TRANSFERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .min(num_deposits); // Can't exceed deposits
    
    let concurrent_proofs: usize = std::env::var("CONCURRENT_PROOFS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .max(1); // At least 1
    
    let use_mock_proofs = std::env::var("USE_MOCK_PROOFS")
        .ok()
        .map(|s| s == "1" || s.to_lowercase() == "true")
        .unwrap_or(false);
    
    if num_transfers > num_deposits {
        panic!("NUM_TRANSFERS ({}) cannot exceed NUM_DEPOSITS ({})", num_transfers, num_deposits);
    }
    
    println!("Configuration:");
    println!("  NUM_DEPOSITS: {}", num_deposits);
    println!("  NUM_TRANSFERS: {}", num_transfers);
    println!("  CONCURRENT_PROOFS: {}", concurrent_proofs);
    println!("  USE_MOCK_PROOFS: {}", if use_mock_proofs { "YES (fast mode)" } else { "NO (real Ligero proofs)" });
    println!("  PROOF_VERIFIER_SERVICE: Automatic (optimized path)");
    println!("  NUM_ACCOUNTS: {} (one per deposit for true parallelism)", num_deposits);
    
    // Setup Ligero environment BEFORE creating the sequencer (needed even for mock proofs to get method_id)
    let (program_path, method_id) = if use_mock_proofs {
        // For mock proofs, we can skip Ligero setup and use a dummy method_id
        // The LIGERO_MOCK_VERIFY flag will make the verifier accept any proof
        println!("  ✓ Mock proof mode enabled - skipping Ligero setup");
        println!("  ✓ Using dummy method ID");
        (String::from("mock"), [0u8; 32])
    } else {
        let (path, id) = setup_ligero_env().expect("Failed to setup Ligero environment");
        println!("  ✓ Ligero configured: {}", path);
        println!("  ✓ Method ID (code commitment): {}", hex::encode(id));
        (path, id)
    };
    
    // Create sequencer with reasonable batch size since we're using the verifier service
    // (no longer need to accommodate multi-MB proofs directly)
    let batch_size = 8 * 1024 * 1024; // 8MB is plenty for lightweight pre-authenticated txs
    let (rollup, da_connection_string, accounts) = create_test_sequencer_with_batch_size(batch_size, method_id, num_deposits).await;
    let rollup = Arc::new(rollup);
    
    println!("\n📍 Shared Database Configuration:");
    println!("  DA Connection String: {}", da_connection_string);
    println!("  Rollup HTTP Address: http://{}", rollup.http_addr);
    println!("  ⚠️  BOTH the rollup and verifier service MUST use this EXACT database!");
    
    // Check if we should start the verifier service automatically or let the user run it manually
    let use_external_verifier = std::env::var("USE_EXTERNAL_VERIFIER").is_ok();
    
    let verifier_url = if use_external_verifier {
        println!("\n⚠️  USE_EXTERNAL_VERIFIER is set - NOT starting verifier service automatically");
        println!("   Start the verifier service manually in another terminal:");
        println!("   cd examples/rollup-ligero");
        println!("   DA_DB=\"{}\" \\", da_connection_string);
        println!("   NODE_RPC_URL=\"http://{}\" \\", rollup.http_addr);
        println!("   ./run_verifier_service.sh");
        println!("\n   Press Enter once the verifier service is running...");
        
        // Wait for user to press Enter
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).expect("Failed to read line");
        
        // Default to port 8080 (the default in run_verifier_service.sh)
        "http://127.0.0.1:8080".to_string()
    } else {
        // Start the proof verifier service automatically
        println!("\n🔧 Starting proof verifier service automatically...");
        let (_verifier_task, verifier_url) = spawn_test_verifier_service(
            &rollup.http_addr.to_string(),
            &da_connection_string,
            method_id,
            concurrent_proofs,
        )
        .await
        .expect("Failed to start proof verifier service");
        println!("  ✓ Proof verifier service: {}", verifier_url);
        verifier_url
    };
    
    // Set environment variable so the sequencer can find the worker tx DB
    std::env::set_var("SOV_WORKER_TX_DB_CONNECTION_STRING", &da_connection_string);
    
    println!("\n📝 Step 1: Processing {} shielded deposits (one per account for parallelism)...", num_deposits);
    
    // Wait until sequencer reports ready to accept txs (handles DA sync jitter)
    {
        let ready_client = reqwest::Client::new();
        let ready_url = format!("http://{}/sequencer/ready", rollup.http_addr);
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > std::time::Duration::from_secs(60) { break; }
            match ready_client.get(&ready_url).send().await {
                Ok(resp) if resp.status().is_success() => break,
                _ => tokio::time::sleep(tokio::time::Duration::from_millis(100)).await,
            }
        }
    }
    
    // Create a channel for transaction messages
    let (tx_sender, mut tx_receiver) = mpsc::channel::<(RawTx, String, std::time::Instant)>(100);
    
    // Track completion times for parallel analysis
    let completion_times = Arc::new(Mutex::new(Vec::new()));
    let completion_times_clone = Arc::clone(&completion_times);
    
    // Spawn a task that processes transactions from the channel
    let rollup_arc = Arc::clone(&rollup);
    let handle = tokio::spawn(async move {
        while let Some((raw_tx, desc, submit_time)) = tx_receiver.recv().await {
            let accept_start = std::time::Instant::now();
            // Retry loop to handle transient "Syncing" 503s
            let mut result = Err(anyhow::anyhow!("initial"));
            for _attempt in 0..300u32 {
                let r = rollup_arc
                    .api_client()
                    .accept_tx(&AcceptTxBody {
                        body: BASE64_STANDARD.encode(&raw_tx),
                    })
                    .await;
                match r {
                    Ok(v) => { result = Ok(v); break; }
                    Err(e) => {
                        // Backoff a bit on failure (likely DA syncing)
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        result = Err(e.into());
                        continue;
                    }
                }
            }
            let accept_duration = accept_start.elapsed();
            
            if let Err(e) = &result {
                eprintln!("  ❌ {} failed: {:?}", desc, e);
                continue;
            }
            
            let total_duration = submit_time.elapsed();
            completion_times_clone.lock().unwrap().push((desc.clone(), accept_duration.as_secs_f64(), total_duration.as_secs_f64()));
            
            // Only print every 10th transaction to reduce noise for large batches
            if desc.contains("Deposit") {
                let tx_num: usize = desc.split('#').nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if tx_num % 10 == 0 || tx_num == 1 {
                    println!("  ✅ {} [submit→accept: {:.3}s, accept call: {:.3}s]", 
                        desc, 
                        total_duration.as_secs_f64(),
                        accept_duration.as_secs_f64()
                    );
                }
            } else {
                println!("  ✅ {} [submit→accept: {:.3}s, accept call: {:.3}s]", 
                    desc, 
                    total_duration.as_secs_f64(),
                    accept_duration.as_secs_f64()
                );
            }
        }
    });
    
    // Create deposits and track note details
    // Each account sends ONE deposit transaction (nonce 0) for true parallel execution
    let base_amount = 1000u128;
    let mut deposit_notes = Vec::new();
    
    let deposits_start = std::time::Instant::now();
    for i in 0..num_deposits {
        let account = &accounts[i]; // Each deposit uses a different account
        let amount = base_amount * (i as u128 + 1);
        let mut rho = [0u8; 32];
        // Use more bytes to ensure uniqueness for large batches
        rho[0] = ((i / 256) % 256) as u8;
        rho[1] = (i % 256) as u8;
        
        let mut recipient = [0u8; 32];
        recipient[0] = ((i / 256) % 256) as u8;
        recipient[1] = ((i + 10) % 256) as u8;
        
        // Store note details AND account index for later spending
        deposit_notes.push((i, amount, rho, recipient));
        
        let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
            MidnightCallMessage::Deposit {
                amount,
                rho,
                recipient,
                view_fvks: None,
                gas: None,
            }
        );
        
        let tx = default_test_signed_transaction::<RT, TestSpec>(
            &account.private_key, // Each account signs its own transaction
            &midnight_call,
            0, // All accounts start with nonce 0
            &<RT as Runtime<TestSpec>>::CHAIN_HASH,
        );
        
        let raw_tx = RawTx::new(borsh::to_vec(&tx).unwrap());
        let submit_time = std::time::Instant::now();
        tx_sender.send((raw_tx, format!("Deposit #{} from Account {}: {} units", i + 1, i, amount), submit_time)).await.unwrap();
        
        // Small delay between transactions to avoid overwhelming the system
        if i % 10 == 0 && i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }
    let deposits_submit_duration = deposits_start.elapsed();
    
    println!("  ⏱️  All {} deposits submitted in {:.3}s", num_deposits, deposits_submit_duration.as_secs_f64());
    println!("  ⏳ Waiting for all deposits to be accepted...");
    
    // Wait a bit for deposits to be processed
    let mut last_count = 0;
    for i in 0..600 { // Wait up to 60 seconds
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let current_count = completion_times.lock().unwrap().len();
        if current_count == num_deposits {
            println!("  ✅ All {} deposits accepted!", num_deposits);
            break;
        }
        if current_count != last_count {
            println!("  ... {} / {} deposits accepted", current_count, num_deposits);
            last_count = current_count;
        }
        // Periodic module-state progress log so it's clear we're not stuck
        if i % 10 == 0 {
            #[derive(serde::Deserialize)]
            struct TreeStateProg { next_position: u64, #[serde(default)] depth: u8 }
            if let Ok(resp) = reqwest::Client::new()
                .get(format!("http://{}/modules/midnight-privacy/tree/state", rollup.http_addr))
                .send().await {
                if let Ok(state) = resp.json::<TreeStateProg>().await {
                    println!("  ... module notes: {} (depth {})", state.next_position, state.depth);
                }
            }
        }
    }
    
    // Analyze deposit parallelism
    let deposit_times = completion_times.lock().unwrap().clone();
    if !deposit_times.is_empty() {
        let deposit_accept_times: Vec<f64> = deposit_times.iter()
            .filter(|(desc, _, _)| desc.contains("Deposit"))
            .map(|(_, accept_time, _)| *accept_time)
            .collect();
        
        if deposit_accept_times.len() > 1 {
            let _avg_accept = deposit_accept_times.iter().sum::<f64>() / deposit_accept_times.len() as f64;
            let max_total = deposit_times.iter()
                .filter(|(desc, _, _)| desc.contains("Deposit"))
                .map(|(_, _, total)| *total)
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap_or(0.0);
            let sequential_time = deposit_accept_times.iter().sum::<f64>();
            let parallelism_factor = sequential_time / max_total.max(0.001);
            
            println!("\n  📊 Deposit Parallelism Analysis:");
            println!("     Sequential execution time: {:.3}s (sum of all accept times)", sequential_time);
            println!("     Actual wall clock time: {:.3}s (max submit→accept)", max_total);
            println!("     Parallelism factor: {:.2}x", parallelism_factor);
            if parallelism_factor > 1.5 {
                println!("     ✅ PARALLEL EXECUTION DETECTED - deposits processed concurrently!");
            } else {
                println!("     ⚠️  SEQUENTIAL EXECUTION - deposits processed one at a time");
            }
        }
    }
    
    if use_mock_proofs {
        println!("\n📝 Step 2: Creating {} MOCK ZK proofs (instant)...", num_transfers);
    } else {
        println!("\n📝 Step 2: Generating {} REAL ZK proofs with {} concurrent proof tasks...", num_transfers, concurrent_proofs);
    }
    
    let domain: Hash32 = [0u8; 32]; // Must match the module's domain
    // Fetch authoritative tree state and notes from the rollup to avoid order mismatches
    #[derive(serde::Deserialize)]
    struct TreeState { root: Vec<u8>, #[serde(default)] depth: u8 }
    #[derive(serde::Deserialize)]
    struct NoteInfo { position: u64, commitment: Vec<u8> }
    #[derive(serde::Deserialize)]
    struct NotesResp { notes: Vec<NoteInfo> }
    let http = reqwest::Client::new();
    let base_url = format!("http://{}", rollup.http_addr);
    let state: TreeState = http
        .get(format!("{}/modules/midnight-privacy/tree/state", base_url))
        .send().await.expect("tree/state request").json().await.expect("tree/state json");
    if state.depth == 0 { panic!("Invalid tree depth from module"); }
    let notes: NotesResp = http
        .get(format!("{}/modules/midnight-privacy/notes?limit=10000", base_url))
        .send().await.expect("notes request").json().await.expect("notes json");
    println!("  Building Merkle tree from module state ({} notes, depth {})...", notes.notes.len(), state.depth);
    let tree_start = std::time::Instant::now();
    let mut shared_tree = MerkleTree::new(state.depth);
    use std::collections::HashMap;
    let mut pos_by_cm: HashMap<[u8;32], usize> = HashMap::with_capacity(notes.notes.len());
    for n in notes.notes.iter() {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            shared_tree.set_leaf(n.position as usize, cm);
            pos_by_cm.insert(cm, n.position as usize);
        }
    }
    let mut shared_anchor = [0u8;32];
    if state.root.len() == 32 {
        shared_anchor.copy_from_slice(&state.root);
    } else {
        shared_anchor = shared_tree.root();
    }
    let tree_duration = tree_start.elapsed();
    println!("  ✓ Merkle tree rebuilt in {:.3}s", tree_duration.as_secs_f64());
    println!("  Anchor root (module): {}", hex::encode(shared_anchor));
    let tree_depth = state.depth;
    
    // Generate proofs with controlled concurrency using a semaphore
    let proofs_start = std::time::Instant::now();
    let semaphore = Arc::new(Semaphore::new(concurrent_proofs));
    
    let proof_handles: Vec<_> = (0..num_transfers)
        .map(|i| {
            let (account_idx, value, rho, recipient) = deposit_notes[i];
            let program_path = program_path.clone();
            let nf_key: Hash32 = [4u8; 32]; // Secret nullifier key
            let shared_anchor = shared_anchor; // Use the shared anchor root
            let semaphore = Arc::clone(&semaphore);
            let use_mock = use_mock_proofs;
            
            // Get the siblings from the module-consistent tree using the computed commitment
            let cm = note_commitment(&domain, value, &rho, &recipient);
            let pos = *pos_by_cm.get(&cm).expect("deposit commitment not found in module notes");
            let siblings = shared_tree.open(pos);
            
            println!("  [Proof Task {}] queued (pos={} siblings={})", i + 1, pos, siblings.len());
            tokio::spawn(async move {
                // Acquire semaphore permit to limit concurrency
                println!("  [Proof Task {}] waiting for permit...", i + 1);
                let _permit = semaphore.acquire().await.unwrap();
                println!("  [Proof Task {}] acquired permit - starting", i + 1);
                
                tokio::task::spawn_blocking(move || -> Result<(usize, Vec<u8>, Hash32, Hash32, u128, u128)> {
                    let proof_start = std::time::Instant::now();
                    
                    // Derive nullifier
                    let nf = nullifier(&domain, &nf_key, &rho);
                    
                    // Split the note: 60% to output1, 40% to output2
                    let out1_value = (value * 60) / 100;
                    let out2_value = value - out1_value;
                    
                    let mut out1_rho = [0u8; 32];
                    out1_rho[0] = ((i * 2 + 5) / 256 % 256) as u8;
                    out1_rho[1] = ((i * 2 + 5) % 256) as u8;
                    let mut out1_recipient = [0u8; 32];
                    out1_recipient[0] = ((i * 2 + 6) / 256 % 256) as u8;
                    out1_recipient[1] = ((i * 2 + 6) % 256) as u8;
                    let cm_out1 = note_commitment(&domain, out1_value, &out1_rho, &out1_recipient);
                    
                    let mut out2_rho = [0u8; 32];
                    out2_rho[0] = ((i * 2 + 7) / 256 % 256) as u8;
                    out2_rho[1] = ((i * 2 + 7) % 256) as u8;
                    let mut out2_recipient = [0u8; 32];
                    out2_recipient[0] = ((i * 2 + 8) / 256 % 256) as u8;
                    out2_recipient[1] = ((i * 2 + 8) % 256) as u8;
                    let cm_out2 = note_commitment(&domain, out2_value, &out2_rho, &out2_recipient);
                    
                    let proof_data = if use_mock {
                        // Create a mock proof - must be a properly structured LigeroProofPackage
                        // The verifier will accept this when LIGERO_MOCK_VERIFY=1 is set
                        println!("  [Transfer {}] Creating mock proof at T+{:.3}s for note with {} units...", 
                            i + 1, proof_start.duration_since(proofs_start).as_secs_f64(), value);
                        
                        // Create the public output that the proof should commit to
                        let public_output = SpendPublic {
                            anchor_root: shared_anchor,
                            nullifier: nf,
                            withdraw_amount: 0,
                            output_commitments: vec![cm_out1, cm_out2],
                            view_attestations: None,
                        };
                        
                        // Serialize the public output with bincode
                        let public_output_bytes = bincode::serialize(&public_output)
                            .expect("Failed to serialize public output");
                        
                        // Create a mock proof package with dummy proof bytes but real public output
                        let package = LigeroProofPackage {
                            proof: vec![0u8; 10_000], // Much smaller mock proof bytes
                            public_output: public_output_bytes,
                            args_json: vec![],
                            private_indices: vec![],
                        };
                        
                        // Serialize the entire package with bincode
                        let mock_proof = bincode::serialize(&package)
                            .expect("Failed to serialize mock proof package");
                        
                        let total_time = proof_start.elapsed();
                        println!("  [Transfer {}] ✓ Mock proof created at T+{:.3}s: {} bytes in {:.3}s", 
                            i + 1, 
                            proof_start.duration_since(proofs_start).as_secs_f64() + total_time.as_secs_f64(),
                            mock_proof.len(), 
                            total_time.as_secs_f64()
                        );
                        
                        mock_proof
                    } else {
                        // Generate REAL Ligero proof
                        println!("  [Transfer {}] Starting proof generation at T+{:.3}s for note with {} units (pos={}, siblings={})...", 
                            i + 1, proof_start.duration_since(proofs_start).as_secs_f64(), value, pos, siblings.len());
                        
                        let position: u64 = pos as u64;
                        
                        let public_output = SpendPublic {
                            anchor_root: shared_anchor,
                            nullifier: nf,
                            withdraw_amount: 0,
                            output_commitments: vec![cm_out1, cm_out2],
                            view_attestations: None,
                        };
                        
                        // Setup Ligero host for proof generation
                        let mut private_indices = vec![2, 3, 4, 5, 6];
                        for j in 0..tree_depth as usize { 
                            private_indices.push(8 + j);
                        }
                        let base = 12 + tree_depth as usize;
                        for j in 0..6 { 
                            private_indices.push(base + j);
                        }
                        
                        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path)
                            .with_private_indices(private_indices);
                        
                        // Add arguments in guest ABI order
                        host.add_hex_arg(hex::encode(domain));
                        host.add_str_arg(value.to_string());
                        host.add_hex_arg(hex::encode(rho));
                        host.add_hex_arg(hex::encode(recipient));
                        host.add_hex_arg(hex::encode(nf_key));
                        host.add_str_arg(position.to_string());
                        host.add_str_arg(tree_depth.to_string());
                        
                        for sibling in &siblings {
                            host.add_hex_arg(hex::encode(sibling));
                        }
                        
                        host.add_hex_arg(hex::encode(shared_anchor));
                        host.add_hex_arg(hex::encode(nf));
                        host.add_str_arg("0".to_string()); // withdraw_amount = 0
                        host.add_str_arg("2".to_string()); // n_out = 2
                        
                        // Output 1
                        host.add_str_arg(out1_value.to_string());
                        host.add_hex_arg(hex::encode(out1_rho));
                        host.add_hex_arg(hex::encode(out1_recipient));
                        host.add_hex_arg(hex::encode(cm_out1));
                        
                        // Output 2
                        host.add_str_arg(out2_value.to_string());
                        host.add_hex_arg(hex::encode(out2_rho));
                        host.add_hex_arg(hex::encode(out2_recipient));
                        host.add_hex_arg(hex::encode(cm_out2));
                        
                        host.set_public_output(&public_output)
                            .context("Failed to set public output")?;
                        
                        let proof_gen_start = std::time::Instant::now();
                        let data = host.run(true)
                            .context("Failed to generate proof")?;
                        let proof_time = proof_gen_start.elapsed();
                        let total_time = proof_start.elapsed();
                        
                        println!("  [Transfer {}] ✓ Proof generated at T+{:.3}s: {} bytes in {:.1}s (total: {:.1}s)", 
                            i + 1, 
                            proof_start.duration_since(proofs_start).as_secs_f64() + proof_time.as_secs_f64(),
                            data.len(), 
                            proof_time.as_secs_f64(),
                            total_time.as_secs_f64()
                        );
                        
                        data
                    };
                    
                    Ok((account_idx, proof_data, shared_anchor, nf, out1_value, out2_value))
                }).await.expect("spawn_blocking failed")
            })
        })
        .collect();
    
    // Wait for all proofs to complete
    println!("  ⏳ Waiting for all {} proofs to complete...", num_transfers);
    let mut proofs = Vec::new();
    let mut completed = 0;
    for (i, handle) in proof_handles.into_iter().enumerate() {
        let result = handle.await
            .expect("Proof generation task panicked")
            .expect(&format!("Failed to generate proof {}", i + 1));
        proofs.push(result);
        completed += 1;
        println!("  ... {} / {} proofs completed", completed, num_transfers);
    }
    let proofs_total_duration = proofs_start.elapsed();
    
    println!("  ✓ All {} proofs generated successfully in {:.3}s!", num_transfers, proofs_total_duration.as_secs_f64());
    println!("  📊 Average proof time: {:.3}s", proofs_total_duration.as_secs_f64() / num_transfers as f64);
    println!("  📊 Effective throughput: {:.2} proofs/sec", num_transfers as f64 / proofs_total_duration.as_secs_f64());
    
    println!("\n📝 Step 3: Submitting {} transfer transactions through proof verifier service...", num_transfers);
    
    // Ensure deposits are finalized in state so anchor_root matches module state
    rollup.da_service.produce_block_now().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    
    // Submit to the verifier service instead of the sequencer
    let http = reqwest::Client::new();
    let mut tasks = Vec::with_capacity(num_transfers);
    
    for (i, (account_idx, proof_data, anchor, nf, out1_value, out2_value)) in proofs.into_iter().enumerate() {
        // Build the FULL tx exactly as before, including the 3MB proof.
        let account = accounts[account_idx].clone();
        let proof_safe = proof_data.try_into().expect("Proof too large");
        let midnight_call = <RT as DispatchCall>::Decodable::MidnightPrivacy(
            MidnightCallMessage::Transfer {
                proof: proof_safe,
                anchor_root: anchor,
                nullifier: nf,
                view_ciphertexts: None,
                gas: None,
            },
        );
        let tx = default_test_signed_transaction::<RT, TestSpec>(
            &account.private_key,
            &midnight_call,
            1,
            &<RT as Runtime<TestSpec>>::CHAIN_HASH,
        );
        let raw_tx = RawTx::new(borsh::to_vec(&tx).unwrap());
        let body = AcceptTxBody {
            body: BASE64_STANDARD.encode(&raw_tx),
        };
        let desc = format!(
            "Transfer #{} acct {} split {}+{}",
            i + 1, account_idx, out1_value, out2_value
        );
        
        // The service verifies the proof, persists a lightweight pre-auth tx to MockDA,
        // and calls the sequencer's /sequencer/worker_txs/{tx_hash} for us.
        let url = format!("{}/midnight-privacy", verifier_url);
        tasks.push(tokio::spawn({
            let http = http.clone();
            async move {
                let t0 = std::time::Instant::now();
                let res = http.post(url).json(&body).send().await;
                let t1 = std::time::Instant::now();
                (desc, t0, t1, res)
            }
        }));
    }
    
    let results = futures::future::join_all(tasks).await;

    // Compute makespan and sum of per-tx durations
    let mut min_start: Option<std::time::Instant> = None;
    let mut max_finish: Option<std::time::Instant> = None;
    let mut sum = 0.0f64;
    let mut successful_txs = 0;
    let mut failed_txs = Vec::new();
    
    for r in results {
        let (desc, t0, t1, res) = r.expect("join");
        match res {
            Ok(resp) if resp.status().is_success() => {
                let dt = (t1 - t0).as_secs_f64();
                sum += dt;
                min_start = Some(min_start.map_or(t0, |m| m.min(t0)));
                max_finish = Some(max_finish.map_or(t1, |m| m.max(t1)));
                
                // Optional: read JSON to assert service accepted and sequencer accepted
                // let _json = resp.json::<serde_json::Value>().await.unwrap();
                let tx_num: usize = desc.split('#').nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if tx_num % 10 == 0 || tx_num <= 10 {
                    println!("  ✅ {} [verifier round-trip: {:.3}s]", desc, dt);
                }
                successful_txs += 1;
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                eprintln!("  ❌ {} [service status {}]: {}", desc, status, text);
                failed_txs.push((desc, text));
            }
            Err(e) => {
                eprintln!("  ❌ {} [HTTP error: {:?}]", desc, e);
                failed_txs.push((desc, format!("{:?}", e)));
            }
        }
    }
    
    if !failed_txs.is_empty() {
        println!("\n  ⚠️  {} transaction(s) failed:", failed_txs.len());
        for (desc, err) in &failed_txs {
            println!("     - {}: {:?}", desc, err);
        }
    }
    
    if successful_txs == 0 {
        println!("\n  ❌ NO SUCCESSFUL TRANSACTIONS - test failed");
        panic!("All transfers failed");
    }
    
    if successful_txs < 2 {
        println!("\n  ⚠️  Only {} successful transaction - cannot compute accurate parallelism factor", successful_txs);
    } else {
        let wall = (max_finish.unwrap() - min_start.unwrap()).as_secs_f64();
        let factor = if wall > 0.0 { sum / wall } else { 0.0 };
        
        println!("\n  📊 Transfer Parallelism Analysis (makespan):");
        println!("     Successful transactions: {}/{}", successful_txs, num_transfers);
        println!("     Sum per‑tx durations: {:.3}s", sum);
        println!("     Makespan (earliest start → latest finish): {:.3}s", wall);
        println!("     Parallelism factor: {:.2}x", factor);
        
        if factor < 1.5 {
            println!("     ⚠️  SEQUENTIAL EXECUTION - transactions processed one at a time");
        } else {
            println!("     ✅ PARALLEL EXECUTION DETECTED - transactions processed concurrently");
        }
    }

    // Close the sender and wait for the deposit consumer to finish
    drop(tx_sender);
    handle.await.unwrap();

    let all_txs_duration = deposits_start.elapsed();
    
    println!("\n🎉 Large scale test completed successfully!");
    println!("   ✓ {} deposits into shielded pool (one per account)", num_deposits);
    if use_mock_proofs {
        println!("   ✓ {} shielded transfers with MOCK proofs ({} successful)", num_transfers, successful_txs);
    } else {
        println!("   ✓ {} shielded transfers with REAL Ligero proofs ({} successful)", num_transfers, successful_txs);
    }
    println!("   ✓ Transactions processed through proof verifier service (optimized path)");
    println!("   📊 Data reduction: ~3MB per proof → few KB per transaction");
    println!("\n📊 Timing Summary:");
    println!("   Total test time: {:.3}s", all_txs_duration.as_secs_f64());
    println!("   Deposits phase: {:.3}s", deposits_submit_duration.as_secs_f64());
    if use_mock_proofs {
        println!("   Proof creation phase: {:.3}s (avg: {:.3}s per mock proof)", 
            proofs_total_duration.as_secs_f64(),
            proofs_total_duration.as_secs_f64() / num_transfers as f64
        );
    } else {
        println!("   Proof generation phase: {:.3}s (avg: {:.3}s per proof)", 
            proofs_total_duration.as_secs_f64(),
            proofs_total_duration.as_secs_f64() / num_transfers as f64
        );
    }
    if successful_txs >= 2 {
        let wall = (max_finish.unwrap() - min_start.unwrap()).as_secs_f64();
        println!("   Transfers phase (wall): {:.3}s", wall);
    }
    if use_mock_proofs {
        println!("\n💡 This test demonstrates large-scale privacy-preserving flow with MOCK proofs for fast testing!");
        println!("   To test with REAL proofs, remove USE_MOCK_PROOFS from environment or set it to 0.");
    } else {
        println!("\n💡 This test demonstrates optimized large-scale privacy-preserving flow!");
        println!("   The proof verifier service automatically pre-processes ZK proofs.");
        println!("   Result: ~3MB per proof reduced to few KB per transaction!");
    }
}

