//! Transfer operation for Midnight Privacy module

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use midnight_privacy::{
    note_commitment, nullifier, CallMessage as MidnightCallMessage, EncryptedNote, Hash32,
    MerkleTree, SpendPublic,
};
use serde::Deserialize;
use sov_address::MultiAddressEvm;
use sov_api_spec::types as api_types;
use sov_ligero_adapter::{Ligero as LigeroAdapter, LigeroProofPackage};
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;
use std::time::{Duration, Instant as StdInstant};
use tokio::time::{sleep, Instant as TokioInstant};

use crate::ligero::{Ligero, LigeroProgramArguments};
use crate::provider::Provider;
use crate::viewer;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

const TREE_DEPTH: u8 = 16;
const DOMAIN: [u8; 32] = [1u8; 32];
const NF_KEY: [u8; 32] = [4u8; 32];
const INCLUSION_POLL_INTERVAL_MS: u64 = 100;
const INCLUSION_TIMEOUT_SECS: u64 = 60;
const INCLUSION_LOG_INTERVAL_SECS: u64 = 5;
const MERKLE_FETCH_LOG_EVERY: usize = 10;
const NOTE_SEARCH_LOG_EVERY: usize = 10;

#[derive(Debug)]
pub struct TransferResult {
    pub tx_hash: String,
    /// Amount sent to destination
    pub amount_sent: u128,
    /// Rho for the output note
    pub output_rho: [u8; 32],
    /// Recipient of the output note
    #[allow(dead_code)]
    pub output_recipient: [u8; 32],
    /// Change amount (if partial transfer)
    pub change_amount: Option<u128>,
    /// Rho for change note (if partial transfer)
    pub change_rho: Option<[u8; 32]>,
    /// Recipient of change note (if partial transfer)
    #[allow(dead_code)]
    pub change_recipient: Option<[u8; 32]>,
}

#[derive(Deserialize, Clone)]
struct TreeState {
    root: Vec<u8>,
    next_position: u64,
}

/// Note information from the rollup API
#[derive(Deserialize, Clone)]
struct NoteInfo {
    position: u64,
    commitment: Vec<u8>,
}

/// Notes response from the rollup API
#[derive(Deserialize)]
struct NotesResp {
    notes: Vec<NoteInfo>,
}

/// Recent roots response from the rollup API
#[derive(Deserialize, Clone)]
struct RootsResp {
    recent_roots: Vec<Hash32>,
}

/// Fetch and rebuild the Merkle tree from the rollup state
async fn fetch_merkle_tree(provider: &Provider) -> Result<(MerkleTree, Hash32)> {
    let start = StdInstant::now();
    tracing::info!("Fetching tree state for Merkle rebuild");
    let state: TreeState = provider
        .query_rest_endpoint("/modules/midnight-privacy/tree/state")
        .await
        .context("Failed to query tree state")?;

    anyhow::ensure!(
        state.root.len() == 32,
        "Tree state root has unexpected length: {}",
        state.root.len()
    );

    let mut tree = MerkleTree::new(TREE_DEPTH);
    let target_leaves = state.next_position as usize;

    tracing::info!(
        "Fetched tree state with next_position={} ({} leaves to fetch)",
        state.next_position,
        target_leaves
    );

    if target_leaves > 0 {
        tree.grow_to_fit(target_leaves);

        // Fetch all notes
        let batch_size = 1000;
        let mut offset = 0;
        let mut batches = 0usize;
        let mut notes_seen = 0usize;

        while offset < target_leaves {
            let endpoint = format!(
                "/modules/midnight-privacy/notes?limit={}&offset={}",
                batch_size, offset
            );
            let batch_resp: NotesResp = provider
                .query_rest_endpoint(&endpoint)
                .await
                .with_context(|| format!("Failed to query notes batch at offset {}", offset))?;

            batches += 1;
            if batch_resp.notes.is_empty() {
                break;
            }

            for n in batch_resp.notes.iter() {
                if n.commitment.len() == 32 {
                    let mut cm = [0u8; 32];
                    cm.copy_from_slice(&n.commitment);
                    if n.position as usize >= tree.len() {
                        tree.grow_to_fit(n.position as usize + 1);
                    }
                    tree.set_leaf(n.position as usize, cm);
                }
            }

            notes_seen += batch_resp.notes.len();
            if batches == 1 || batches % MERKLE_FETCH_LOG_EVERY == 0 || notes_seen >= target_leaves
            {
                tracing::info!(
                    batch = batches,
                    seen = notes_seen,
                    target = target_leaves,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Fetched note commitments while rebuilding Merkle tree"
                );
            } else {
                tracing::debug!(
                    batch = batches,
                    seen = notes_seen,
                    target = target_leaves,
                    "Fetched note commitments batch while rebuilding Merkle tree"
                );
            }

            offset += batch_resp.notes.len();
        }
    }

    let mut root = [0u8; 32];
    root.copy_from_slice(&state.root);

    tracing::debug!(
        "Rebuilt Merkle tree: {} leaves, root={}",
        tree.len(),
        hex::encode(&root)
    );
    tracing::info!(
        elapsed_ms = start.elapsed().as_millis(),
        leaves = tree.len(),
        "Finished rebuilding Merkle tree"
    );

    Ok((tree, root))
}

/// Find a note in the tree by its commitment
async fn find_note_position(provider: &Provider, note_commitment: [u8; 32]) -> Result<Option<u64>> {
    let search_start = StdInstant::now();
    let commitment_hex = hex::encode(note_commitment);
    tracing::info!("Searching for input note commitment {}", commitment_hex);
    let batch_size = 1000;
    let mut offset = 0;
    let mut batches = 0usize;
    let mut scanned = 0usize;

    loop {
        let endpoint = format!(
            "/modules/midnight-privacy/notes?limit={}&offset={}",
            batch_size, offset
        );
        let batch_resp: NotesResp = provider
            .query_rest_endpoint(&endpoint)
            .await
            .with_context(|| format!("Failed to query notes batch at offset {}", offset))?;

        batches += 1;
        let len = batch_resp.notes.len();
        scanned += len;

        for n in batch_resp.notes.iter() {
            if n.commitment.len() == 32 {
                let mut cm = [0u8; 32];
                cm.copy_from_slice(&n.commitment);
                if cm == note_commitment {
                    tracing::info!(
                        position = n.position,
                        batches_scanned = batches,
                        elapsed_ms = search_start.elapsed().as_millis(),
                        "Found input note commitment in tree"
                    );
                    return Ok(Some(n.position));
                }
            }
        }

        if batches == 1 || batches % NOTE_SEARCH_LOG_EVERY == 0 || len < batch_size {
            tracing::info!(
                batches_scanned = batches,
                notes_scanned = scanned,
                elapsed_ms = search_start.elapsed().as_millis(),
                "Scanning notes for input commitment"
            );
        } else {
            tracing::debug!(
                batches_scanned = batches,
                notes_scanned = scanned,
                "Scanning notes for input commitment"
            );
        }

        if len < batch_size {
            tracing::warn!(
                batches_scanned = batches,
                notes_scanned = scanned,
                elapsed_ms = search_start.elapsed().as_millis(),
                target_commitment = commitment_hex,
                "Finished scanning notes without finding input commitment. \
                 Total notes in tree: {}. Enable DEBUG logging to see all commitments.",
                scanned
            );

            // Log first few commitments at INFO level to help debug
            if scanned > 0 && scanned <= 10 {
                tracing::info!("Notes found in tree (showing up to 10):");
                // Re-fetch first batch to show commitments
                let first_batch: NotesResp = provider
                    .query_rest_endpoint("/modules/midnight-privacy/notes?limit=10&offset=0")
                    .await
                    .ok()
                    .unwrap_or_else(|| NotesResp { notes: vec![] });

                for (i, n) in first_batch.notes.iter().enumerate() {
                    if n.commitment.len() == 32 {
                        tracing::info!(
                            "  Note {}: position={}, commitment={}",
                            i,
                            n.position,
                            hex::encode(&n.commitment)
                        );
                    }
                }
            }
            break;
        }
        offset += batch_size;
    }

    Ok(None)
}

/// Get a recent valid anchor root
async fn get_anchor_root(provider: &Provider) -> Result<Hash32> {
    let start = StdInstant::now();
    tracing::info!("Fetching recent anchor root");
    let roots_state: RootsResp = provider
        .query_rest_endpoint("/modules/midnight-privacy/roots/recent")
        .await
        .context("Failed to query recent roots")?;

    let anchor = roots_state
        .recent_roots
        .last()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("No recent roots available"))?;

    tracing::info!(
        elapsed_ms = start.elapsed().as_millis(),
        anchor_root = %hex::encode(anchor),
        "Fetched anchor root"
    );

    Ok(anchor)
}

/// Poll the ledger for transaction inclusion
async fn wait_for_inclusion(provider: &Provider, tx_hash: &str) -> Result<()> {
    let start = TokioInstant::now();
    let deadline = start + Duration::from_secs(INCLUSION_TIMEOUT_SECS);
    let mut last_log = start;
    let mut attempts: u64 = 0;

    loop {
        attempts += 1;
        if TokioInstant::now() > deadline {
            anyhow::bail!("Timeout waiting for transaction {} to be included", tx_hash);
        }

        match provider
            .query_rest_endpoint::<api_types::LedgerTx>(&format!(
                "/ledger/txs/{}?children=1",
                tx_hash
            ))
            .await
        {
            Ok(ltx) => {
                if ltx.receipt.result != api_types::TxReceiptResult::Successful {
                    anyhow::bail!(
                        "Transaction {} included but not successful: {:?}",
                        tx_hash,
                        ltx.receipt
                    );
                }
                tracing::info!(
                    tx_hash,
                    block = ltx.batch_number,
                    attempts,
                    waited_ms = start.elapsed().as_millis(),
                    "Transaction included successfully"
                );
                return Ok(());
            }
            Err(err) => {
                if last_log.elapsed() >= Duration::from_secs(INCLUSION_LOG_INTERVAL_SECS) {
                    let remaining_secs = deadline
                        .checked_duration_since(TokioInstant::now())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    tracing::info!(
                        tx_hash,
                        attempts,
                        waited_ms = start.elapsed().as_millis(),
                        remaining_secs,
                        "Waiting for transaction inclusion"
                    );
                    tracing::debug!(tx_hash, attempts, error = %err, "Latest inclusion check failed");
                    last_log = TokioInstant::now();
                }
                sleep(Duration::from_millis(INCLUSION_POLL_INTERVAL_MS)).await;
            }
        }
    }
}

/// Create an unsigned transaction for shielded transfer
async fn create_transfer_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    proof_bytes: Vec<u8>,
    anchor_root: Hash32,
    nullifier: Hash32,
    view_ciphertexts: Option<Vec<EncryptedNote>>,
) -> Result<UnsignedTransaction<McpRuntime, McpSpec>> {
    let chain_data = provider
        .get_chain_data()
        .await
        .context("Failed to fetch chain data from rollup")?;

    let chain_id = chain_data.chain_id;

    let safe_proof = proof_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large for SafeVec"))?;

    let transfer_call = MidnightCallMessage::<McpSpec>::Transfer {
        proof: safe_proof,
        anchor_root,
        nullifiers: vec![nullifier],
        view_ciphertexts,
        recipient_ciphertexts: None, // TODO: Add recipient ciphertext support
        gas: None,
    };

    let runtime_call = demo_stf::runtime::RuntimeCall::<McpSpec>::MidnightPrivacy(transfer_call);

    let public_key = wallet
        .default_public_key()
        .context("Failed to get public key from wallet")?;

    let nonce = provider
        .get_nonce::<McpSpec>(&public_key)
        .await
        .context("Failed to get nonce from provider")?;

    let generation = if nonce == 0 {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        timestamp
    } else {
        nonce
    };

    let max_fee = Amount::from(1_000_000_000_000u128);

    let unsigned_tx = UnsignedTransaction::<McpRuntime, McpSpec>::new(
        runtime_call,
        chain_id,
        PriorityFeeBips::ZERO,
        max_fee,
        UniquenessData::Generation(generation),
        None,
    );

    Ok(unsigned_tx)
}

/// Transfer funds within the Midnight Privacy shielded pool
/// 
/// If `send_amount` < `note_value`, creates 2 outputs:
///   - Output 0: `send_amount` → `output_recipient` (destination)
///   - Output 1: `note_value - send_amount` → `change_recipient` (change back to sender)
/// 
/// If `send_amount` == `note_value`, creates 1 output (full transfer, no change).
pub async fn transfer(
    ligero: &Ligero,
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    note_value: u128,
    send_amount: u128,
    input_rho: [u8; 32],
    input_recipient: [u8; 32],
    output_recipient: [u8; 32],
    change_recipient: Option<[u8; 32]>,
) -> Result<TransferResult> {
    // Validate amounts
    if send_amount == 0 {
        anyhow::bail!("send_amount must be greater than 0");
    }
    if send_amount > note_value {
        anyhow::bail!(
            "send_amount ({}) exceeds note value ({})",
            send_amount,
            note_value
        );
    }

    let has_change = send_amount < note_value;
    let change_amount = if has_change { note_value - send_amount } else { 0 };

    if has_change && change_recipient.is_none() {
        anyhow::bail!("change_recipient is required when send_amount < note_value");
    }

    tracing::info!(
        "Starting transfer: note_value={}, send_amount={}, change_amount={}",
        note_value,
        send_amount,
        change_amount
    );
    let overall_start = StdInstant::now();

    // Step 1: Fetch Merkle tree and find the note
    let tree_start = StdInstant::now();
    let (tree, _current_root) = fetch_merkle_tree(provider).await?;
    tracing::info!(
        elapsed_ms = tree_start.elapsed().as_millis(),
        "Merkle tree fetch and rebuild completed"
    );

    let input_cm = note_commitment(&DOMAIN, note_value, &input_rho, &input_recipient);
    tracing::info!(
        "Looking for note with commitment: {}, computed from value={}, rho={}, recipient={}",
        hex::encode(&input_cm),
        note_value,
        hex::encode(&input_rho),
        hex::encode(&input_recipient)
    );

    let position_start = StdInstant::now();
    let position = find_note_position(provider, input_cm)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Input note not found in tree. Searched for commitment: {}. \
                 This could mean: (1) the deposit transaction hasn't been included yet, \
                 (2) the verifier is in defer mode and needs /midnight-privacy/flush, \
                 (3) wrong rho/recipient values were provided, \
                 (4) wrong value amount",
                hex::encode(&input_cm)
            )
        })?;

    tracing::info!(
        position,
        elapsed_ms = position_start.elapsed().as_millis(),
        "Found input note position"
    );

    // Step 2: Get anchor root
    let anchor_start = StdInstant::now();
    let anchor_root = get_anchor_root(provider).await?;
    tracing::info!(
        elapsed_ms = anchor_start.elapsed().as_millis(),
        anchor_root = %hex::encode(anchor_root),
        "Anchor root ready"
    );

    // Step 3: Generate output note parameters
    // Output 0: send_amount → output_recipient (destination)
    let out_rho_0: [u8; 32] = rand::random();
    let out_recipient_0: [u8; 32] = output_recipient;
    let cm_out_0 = note_commitment(&DOMAIN, send_amount, &out_rho_0, &out_recipient_0);

    // Output 1 (optional): change → change_recipient (back to sender)
    let (out_rho_1, out_recipient_1, cm_out_1) = if has_change {
        let rho: [u8; 32] = rand::random();
        let recipient = change_recipient.unwrap();
        let cm = note_commitment(&DOMAIN, change_amount, &rho, &recipient);
        (Some(rho), Some(recipient), Some(cm))
    } else {
        (None, None, None)
    };

    let num_outputs: u32 = if has_change { 2 } else { 1 };

    // Step 4: Compute nullifier
    let nf = nullifier(&DOMAIN, &NF_KEY, &input_rho);

    // Step 4b: Load authority VFK and create viewer bundles if configured
    let authority_vfk = viewer::load_authority_vfk();
    let (view_attestations, view_ciphertexts) = if let Some(vfk) = authority_vfk {
        tracing::info!(
            "Authority VFK configured: generating viewer attestations for {} output(s)",
            num_outputs
        );
        
        // sender_id for transfers is the input_recipient (spender's address)
        let (att_0, enc_0) = viewer::make_viewer_bundle(
            &vfk,
            &DOMAIN,
            send_amount,
            &out_rho_0,
            &out_recipient_0,
            &input_recipient,
            &cm_out_0,
        );

        if has_change {
            let (att_1, enc_1) = viewer::make_viewer_bundle(
                &vfk,
                &DOMAIN,
                change_amount,
                out_rho_1.as_ref().unwrap(),
                out_recipient_1.as_ref().unwrap(),
                &input_recipient,
                cm_out_1.as_ref().unwrap(),
            );
            (Some(vec![att_0, att_1]), Some(vec![enc_0, enc_1]))
        } else {
            (Some(vec![att_0]), Some(vec![enc_0]))
        }
    } else {
        tracing::debug!(
            "No authority VFK configured: transfer will not include viewer attestation"
        );
        (None, None)
    };

    // Note: The webgpu_prover generates the proof AND packages it with the public output
    // (SpendPublic) internally, so we don't need to create it here.

    // Step 5: Generate ZK proof
    tracing::info!("Generating ZK proof with {} output(s)...", num_outputs);

    let siblings = tree.open(position as usize);
    let depth = siblings.len() as u32;

    // Build private indices for Ligero (0-indexed, matching continuous_transfers.rs)
    // Indices 2-6: value, in_rho, in_recipient, nf_key, position
    let mut private_indices: Vec<u32> = vec![2, 3, 4, 5, 6];
    // Indices 7..(7+depth-1): Merkle siblings
    for j in 0..depth {
        private_indices.push(7 + j);
    }
    // Outputs start at 11+depth; mark value/rho/recipient for each output as private
    let output_base = 11 + depth;
    for out_idx in 0..num_outputs {
        let base = output_base + 4 * out_idx;
        private_indices.extend_from_slice(&[base, base + 1, base + 2]);
    }

    // Viewer section: vfk is private (if authority VFK is configured)
    if authority_vfk.is_some() {
        let base_after_outs = 12 + depth + 4 * num_outputs;
        let vfk_arg_index = base_after_outs + 2;
        private_indices.push(vfk_arg_index);
    }

    // Prepare proof arguments with correct HEX/STR format
    // Hash values use HEX format, numeric values use STR format
    let mut proof_args: Vec<LigeroProgramArguments> = vec![
        LigeroProgramArguments::HEX {
            hex: hex::encode(DOMAIN),
        }, // domain
        LigeroProgramArguments::STR {
            str: note_value.to_string(),
        }, // value (input note value)
        LigeroProgramArguments::HEX {
            hex: hex::encode(input_rho),
        }, // in_rho
        LigeroProgramArguments::HEX {
            hex: hex::encode(input_recipient),
        }, // in_recipient
        LigeroProgramArguments::HEX {
            hex: hex::encode(NF_KEY),
        }, // nf_key
        LigeroProgramArguments::STR {
            str: position.to_string(),
        }, // position
        LigeroProgramArguments::STR {
            str: depth.to_string(),
        }, // depth
    ];

    // Add siblings as HEX
    for s in &siblings {
        proof_args.push(LigeroProgramArguments::HEX {
            hex: hex::encode(s),
        });
    }

    // Add remaining public inputs
    proof_args.extend_from_slice(&[
        LigeroProgramArguments::HEX {
            hex: hex::encode(anchor_root),
        }, // anchor
        LigeroProgramArguments::HEX {
            hex: hex::encode(nf),
        }, // nullifier
        LigeroProgramArguments::STR {
            str: "0".to_string(),
        }, // withdraw_amount
        LigeroProgramArguments::STR {
            str: num_outputs.to_string(),
        }, // num_outputs
    ]);

    // Output 0: destination (send_amount → output_recipient)
    proof_args.extend_from_slice(&[
        LigeroProgramArguments::STR {
            str: send_amount.to_string(),
        }, // output 0 value
        LigeroProgramArguments::HEX {
            hex: hex::encode(out_rho_0),
        }, // output 0 rho
        LigeroProgramArguments::HEX {
            hex: hex::encode(out_recipient_0),
        }, // output 0 recipient
        LigeroProgramArguments::HEX {
            hex: hex::encode(cm_out_0),
        }, // output 0 commitment
    ]);

    // Output 1: change (if partial transfer)
    if has_change {
        proof_args.extend_from_slice(&[
            LigeroProgramArguments::STR {
                str: change_amount.to_string(),
            }, // output 1 value
            LigeroProgramArguments::HEX {
                hex: hex::encode(out_rho_1.unwrap()),
            }, // output 1 rho
            LigeroProgramArguments::HEX {
                hex: hex::encode(out_recipient_1.unwrap()),
            }, // output 1 recipient
            LigeroProgramArguments::HEX {
                hex: hex::encode(cm_out_1.unwrap()),
            }, // output 1 commitment
        ]);
    }

    // Add viewer section arguments if authority VFK is configured
    if let (Some(vfk), Some(ref atts)) = (authority_vfk, &view_attestations) {
        if let Some(att) = atts.first() {
            proof_args.extend_from_slice(&[
                LigeroProgramArguments::STR {
                    str: "1".to_string(),
                }, // m_viewers
                LigeroProgramArguments::HEX {
                    hex: hex::encode(att.fvk_commitment),
                }, // vfk_commitment
                LigeroProgramArguments::HEX {
                    hex: hex::encode(vfk),
                }, // vfk (private)
                LigeroProgramArguments::HEX {
                    hex: hex::encode(att.ct_hash),
                }, // ct_hash
                LigeroProgramArguments::HEX {
                    hex: hex::encode(att.mac),
                }, // mac
            ]);
        }
    }

    // Save args/private indices for packaging (verifier expects a LigeroProofPackage)
    let proof_args_for_package = proof_args.clone();
    let private_indices_for_package: Vec<usize> =
        private_indices.iter().map(|i| (*i as usize) + 1).collect();

    let (packing, gpu_threads) = ligero.resolve_prover_params(8192, None);
    let proof_start = StdInstant::now();
    let proof_bytes_raw = ligero
        .generate_proof(packing, gpu_threads, private_indices, proof_args)
        .inspect_err(|e| tracing::error!("Failed to generate Ligero proof for transfer: {:?}", e))
        .context("Failed to generate Ligero proof for transfer")?;

    tracing::info!(
        elapsed_ms = proof_start.elapsed().as_millis(),
        proof_bytes_len = proof_bytes_raw.len(),
        "Generated proof bytes"
    );

    // Package proof with public outputs (SpendPublic) for verifier compatibility
    let mut output_commitments = vec![cm_out_0];
    if has_change {
        output_commitments.push(cm_out_1.unwrap());
    }
    let public_output = SpendPublic {
        anchor_root,
        nullifiers: vec![nf],
        withdraw_amount: 0, // pure shielded transfer, no transparent withdrawal
        output_commitments,
        view_attestations,
        recipient_attestations: None, // TODO: Add recipient attestation support
    };

    let proof_package = LigeroProofPackage {
        proof: proof_bytes_raw,
        public_output: bincode::serialize(&public_output)
            .context("Failed to serialize spend public output")?,
        args_json: serde_json::to_vec(&proof_args_for_package)
            .context("Failed to serialize Ligero args for package")?,
        private_indices: private_indices_for_package,
    };

    let proof_bytes =
        bincode::serialize(&proof_package).context("Failed to serialize Ligero proof package")?;
    tracing::debug!(
        proof_package_len = proof_bytes.len(),
        "Serialized Ligero proof package for submission"
    );

    // Step 6: Create and sign transaction
    let unsigned_tx_start = StdInstant::now();
    let unsigned_tx = create_transfer_unsigned_tx(
        provider,
        wallet,
        proof_bytes,
        anchor_root,
        nf,
        view_ciphertexts,
    )
    .await?;
    tracing::info!(
        elapsed_ms = unsigned_tx_start.elapsed().as_millis(),
        "Unsigned transfer transaction created"
    );

    let sign_start = StdInstant::now();
    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .context("Failed to sign transaction")?;

    tracing::info!(
        elapsed_ms = sign_start.elapsed().as_millis(),
        tx_bytes_len = raw_tx.len(),
        "Transaction signed"
    );

    // Step 7: Submit transaction to verifier service
    let submit_start = StdInstant::now();
    let tx_hash = provider
        .submit_to_verifier(raw_tx)
        .await
        .context("Failed to submit transaction to verifier service")?;

    tracing::info!(
        elapsed_ms = submit_start.elapsed().as_millis(),
        tx_hash,
        "Transfer transaction submitted via verifier service"
    );

    // Step 8: Poll for inclusion
    tracing::info!("Waiting for transaction inclusion...");
    wait_for_inclusion(provider, &tx_hash).await?;
    tracing::info!(
        elapsed_ms = overall_start.elapsed().as_millis(),
        tx_hash,
        "Transfer operation completed"
    );

    Ok(TransferResult {
        tx_hash,
        amount_sent: send_amount,
        output_rho: out_rho_0,
        output_recipient: out_recipient_0,
        change_amount: if has_change { Some(change_amount) } else { None },
        change_rho: out_rho_1,
        change_recipient: out_recipient_1,
    })
}
