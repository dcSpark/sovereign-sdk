//! Transfer operation for Midnight Privacy module

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use demo_stf::runtime::Runtime;
use midnight_privacy::{
    nf_key_from_sk, note_commitment, nullifier, pk_from_sk, recipient_from_pk_v2,
    recipient_from_sk_v2, CallMessage as MidnightCallMessage, EncryptedNote, Hash32, MerkleTree,
    PrivacyAddress, SpendPublic,
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
use std::collections::HashMap;
use std::time::{Duration, Instant as StdInstant};
use tokio::time::{sleep, Instant as TokioInstant};

use crate::ligero::{Ligero, LigeroProgramArguments};
use crate::fvk_service::ViewerFvkBundle;
use crate::operations::DEFAULT_MAX_FEE;
use crate::provider::Provider;
use crate::viewer;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<MockDaSpec, LigeroAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

const TREE_DEPTH: u8 = 16;
const DOMAIN: [u8; 32] = [1u8; 32];
const INCLUSION_POLL_INTERVAL_MS: u64 = 100;
const INCLUSION_TIMEOUT_SECS: u64 = 60;
const INCLUSION_LOG_INTERVAL_SECS: u64 = 5;
const MERKLE_FETCH_LOG_EVERY: usize = 10;

#[derive(Debug)]
pub struct TransferResult {
    pub tx_hash: String,
    pub created_at: i64,
    /// Amount sent to destination
    #[allow(dead_code)]
    pub amount_sent: u128,
    /// Rho for the output note
    #[allow(dead_code)]
    pub output_rho: [u8; 32],
    /// Recipient of the output note
    #[allow(dead_code)]
    pub output_recipient: [u8; 32],
    /// Change amount (if partial transfer)
    #[allow(dead_code)]
    pub change_amount: Option<u128>,
    /// Rho for change note (if partial transfer)
    #[allow(dead_code)]
    pub change_rho: Option<[u8; 32]>,
    /// Recipient of change note (if partial transfer)
    #[allow(dead_code)]
    pub change_recipient: Option<[u8; 32]>,
}

/// A spendable input note (owned by the same `(spend_sk, pk_ivk_owner)`).
#[derive(Debug, Clone)]
pub struct TransferInputNote {
    pub value: u128,
    pub rho: Hash32,
    pub sender_id: Hash32,
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

/// Fetch and rebuild the Merkle tree from the rollup state
async fn fetch_merkle_tree(provider: &Provider) -> Result<(MerkleTree, Hash32, HashMap<Hash32, u64>)> {
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
    let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();
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
                    pos_by_cm.insert(cm, n.position);
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

    Ok((tree, root, pos_by_cm))
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

/// Create an unsigned transaction for shielded transfer (multi-input, up to 4 nullifiers).
async fn create_transfer_unsigned_tx(
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    proof_bytes: Vec<u8>,
    anchor_root: Hash32,
    nullifiers: Vec<Hash32>,
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
        nullifiers,
        view_ciphertexts,
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

    let max_fee = Amount::from(DEFAULT_MAX_FEE);

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

/// Transfer funds within the Midnight Privacy shielded pool (multi-input, up to 4 inputs).
///
/// Uses `inputs` (largest-first, typically) to fund `send_amount`.
///
/// If `send_amount` < `sum(inputs)`, creates 2 outputs:
///   - Output 0: `send_amount` → destination
///   - Output 1: `sum(inputs) - send_amount` → change back to sender
///
/// If `send_amount` == `sum(inputs)`, creates 1 output (no change).
pub async fn transfer(
    ligero: &Ligero,
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    spend_sk: Hash32,
    pk_ivk_owner: Hash32,
    send_amount: u128,
    inputs: Vec<TransferInputNote>,
    destination_pk_spend: Hash32,
    destination_pk_ivk: Hash32,
    viewer_fvk_bundle: Option<ViewerFvkBundle>,
) -> Result<TransferResult> {
    // Validate amounts
    if send_amount == 0 {
        anyhow::bail!("send_amount must be greater than 0");
    }
    anyhow::ensure!(!inputs.is_empty(), "at least 1 input note is required");
    anyhow::ensure!(
        inputs.len() <= viewer::MAX_INS,
        "at most {} input notes are supported",
        viewer::MAX_INS
    );

    let mut in_values_u64: Vec<u64> = Vec::with_capacity(inputs.len());
    let mut in_rhos: Vec<Hash32> = Vec::with_capacity(inputs.len());
    let mut in_sender_ids: Vec<Hash32> = Vec::with_capacity(inputs.len());
    let mut sum_in_u64: u64 = 0;
    for note in &inputs {
        let v_u64: u64 = note
            .value
            .try_into()
            .context("input note value does not fit into u64 (required by note_spend_guest v2)")?;
        sum_in_u64 = sum_in_u64
            .checked_add(v_u64)
            .ok_or_else(|| anyhow::anyhow!("sum of input values overflows u64"))?;
        in_values_u64.push(v_u64);
        in_rhos.push(note.rho);
        in_sender_ids.push(note.sender_id);
    }

    let send_amount_u64: u64 = send_amount
        .try_into()
        .context("send_amount does not fit into u64 (required by note_spend_guest v2)")?;
    if send_amount_u64 > sum_in_u64 {
        anyhow::bail!(
            "send_amount ({}) exceeds sum(inputs) ({})",
            send_amount,
            sum_in_u64
        );
    }

    let has_change = send_amount_u64 < sum_in_u64;
    let change_amount = if has_change {
        (sum_in_u64 - send_amount_u64) as u128
    } else {
        0
    };
    let change_amount_u64: u64 = change_amount
        .try_into()
        .context("change_amount does not fit into u64 (required by note_spend_guest v2)")?;

    tracing::info!(
        "Starting transfer: n_in={}, sum_in={}, send_amount={}, change_amount={}",
        inputs.len(),
        sum_in_u64,
        send_amount,
        change_amount
    );
    let overall_start = StdInstant::now();

    // Derive the spender's privacy recipient (owner address) from (spend_sk, pk_ivk_owner).
    // This matches note_spend_guest v2, where the input recipient is derived in-circuit.
    let input_recipient = recipient_from_sk_v2(&DOMAIN, &spend_sk, &pk_ivk_owner);
    let sender_id_out = input_recipient;
    let pk_spend_owner = pk_from_sk(&spend_sk);

    // Step 1: Fetch Merkle tree and build commitment -> position map.
    let tree_start = StdInstant::now();
    let (tree, anchor_root, pos_by_cm) = fetch_merkle_tree(provider).await?;
    tracing::info!(
        elapsed_ms = tree_start.elapsed().as_millis(),
        anchor_root = %hex::encode(anchor_root),
        "Merkle tree fetch and rebuild completed"
    );

    // Resolve positions + auth paths for each input.
    let mut input_cms: Vec<Hash32> = Vec::with_capacity(inputs.len());
    let mut positions: Vec<u64> = Vec::with_capacity(inputs.len());
    let mut siblings_by_input: Vec<Vec<Hash32>> = Vec::with_capacity(inputs.len());

    for i in 0..inputs.len() {
        let cm_i = note_commitment(
            &DOMAIN,
            in_values_u64[i],
            &in_rhos[i],
            &input_recipient,
            &in_sender_ids[i],
        );
        let pos_i = *pos_by_cm.get(&cm_i).ok_or_else(|| {
            anyhow::anyhow!(
                "Input note not found in tree. Searched for commitment: {}",
                hex::encode(cm_i)
            )
        })?;
        let siblings_i = tree.open(pos_i as usize);

        input_cms.push(cm_i);
        positions.push(pos_i);
        siblings_by_input.push(siblings_i);
    }

    let depth = siblings_by_input
        .first()
        .map(|s| s.len())
        .unwrap_or(0);
    anyhow::ensure!(depth > 0, "Merkle tree depth is zero");
    anyhow::ensure!(
        siblings_by_input.iter().all(|s| s.len() == depth),
        "inconsistent Merkle path depth across inputs"
    );

    // Step 3: Generate output note parameters
    // Output 0: send_amount → destination recipient (derived from destination keys)
    let out_rho_0: [u8; 32] = rand::random();
    let out_recipient_0: [u8; 32] =
        recipient_from_pk_v2(&DOMAIN, &destination_pk_spend, &destination_pk_ivk);
    let cm_out_0 = note_commitment(
        &DOMAIN,
        send_amount_u64,
        &out_rho_0,
        &out_recipient_0,
        &sender_id_out,
    );

    // Output 1 (optional): change → back to sender (recipient derived from owner keys)
    let (out_rho_1, out_recipient_1, cm_out_1) = if has_change {
        let rho: [u8; 32] = rand::random();
        let recipient = recipient_from_pk_v2(&DOMAIN, &pk_spend_owner, &pk_ivk_owner);
        let cm = note_commitment(&DOMAIN, change_amount_u64, &rho, &recipient, &sender_id_out);
        (Some(rho), Some(recipient), Some(cm))
    } else {
        (None, None, None)
    };

    let num_outputs: u32 = if has_change { 2 } else { 1 };

    // Step 4: Compute nullifiers (one per input)
    let nf_key = nf_key_from_sk(&DOMAIN, &spend_sk);
    let nullifiers: Vec<Hash32> = in_rhos
        .iter()
        .map(|rho| nullifier(&DOMAIN, &nf_key, rho))
        .collect();

    // Step 4b: Create viewer bundles if a viewer FVK bundle is provided
    let (view_attestations, view_ciphertexts) = if let Some(ref bundle) = viewer_fvk_bundle {
        let fvk = bundle.fvk;
        tracing::info!(
            "Viewer FVK configured: generating viewer attestations for {} output(s)",
            num_outputs
        );

        let mut cm_ins: [Hash32; viewer::MAX_INS] = [[0u8; 32]; viewer::MAX_INS];
        for (i, cm) in input_cms.iter().enumerate().take(viewer::MAX_INS) {
            cm_ins[i] = *cm;
        }
        let (att_0, enc_0) = viewer::make_viewer_bundle(
            &fvk,
            &DOMAIN,
            send_amount,
            &out_rho_0,
            &out_recipient_0,
            &sender_id_out,
            &cm_ins,
            &cm_out_0,
        )?;

        if has_change {
            let (att_1, enc_1) = viewer::make_viewer_bundle(
                &fvk,
                &DOMAIN,
                change_amount,
                out_rho_1.as_ref().unwrap(),
                out_recipient_1.as_ref().unwrap(),
                &sender_id_out,
                &cm_ins,
                cm_out_1.as_ref().unwrap(),
            )?;
            (Some(vec![att_0, att_1]), Some(vec![enc_0, enc_1]))
        } else {
            (Some(vec![att_0]), Some(vec![enc_0]))
        }
    } else {
        tracing::debug!(
            "No authority FVK configured: transfer will not include viewer attestation"
        );
        (None, None)
    };

    // Step 4c: Fetch deny-map (blacklist) root + Merkle openings.
    //
    // The spend circuit binds to `blacklist_root` as a public input and requires BL_DEPTH sibling
    // paths (private) for:
    // - sender (spender identity)
    // - each output recipient
    let sender_addr = PrivacyAddress::from_keys(&pk_spend_owner, &pk_ivk_owner);
    let dest_addr = PrivacyAddress::from_keys(&destination_pk_spend, &destination_pk_ivk);

    let (sender_opening, dest_opening) = if sender_addr == dest_addr {
        let opening: midnight_privacy::BlacklistOpeningResponse = provider
            .query_rest_endpoint(&format!(
                "/modules/midnight-privacy/blacklist/opening/{sender_addr}"
            ))
            .await
            .context("Failed to query deny-map opening for sender/destination")?;
        (opening.clone(), opening)
    } else {
        tokio::try_join!(
            async {
                provider
                    .query_rest_endpoint::<midnight_privacy::BlacklistOpeningResponse>(&format!(
                        "/modules/midnight-privacy/blacklist/opening/{sender_addr}"
                    ))
                    .await
            },
            async {
                provider
                    .query_rest_endpoint::<midnight_privacy::BlacklistOpeningResponse>(&format!(
                        "/modules/midnight-privacy/blacklist/opening/{dest_addr}"
                    ))
                    .await
            }
        )
        .context("Failed to query deny-map openings")?
    };

    anyhow::ensure!(
        sender_opening.blacklist_root == dest_opening.blacklist_root,
        "Deny-map root changed while fetching openings (sender vs destination)"
    );
    let blacklist_root = sender_opening.blacklist_root;

    if sender_opening.is_blacklisted {
        anyhow::bail!("Sender privacy address is frozen (blacklisted)");
    }
    if dest_opening.is_blacklisted {
        anyhow::bail!("Destination privacy address is frozen (blacklisted)");
    }

    // Note: The proof service generates raw proof bytes; we still package the proof
    // with the public output (SpendPublic) for verifier compatibility.

    // Step 5: Generate ZK proof
    tracing::info!("Generating ZK proof with {} output(s)...", num_outputs);

    use ligetron::bn254fr_native::submod_checked;
    use ligetron::Bn254Fr;

    fn bn254fr_from_hash32_be(h: &Hash32) -> Bn254Fr {
        let mut out = Bn254Fr::new();
        out.set_bytes_big(h);
        out
    }

    fn inv_enforce_v2(
        in_values: &[u64],
        in_rhos: &[Hash32],
        out_values: &[u64],
        out_rhos: &[Hash32],
    ) -> Hash32 {
        let mut enforce_prod = Bn254Fr::from_u32(1);

        for v in in_values {
            enforce_prod.mulmod_checked(&Bn254Fr::from_u64(*v));
        }
        for v in out_values {
            enforce_prod.mulmod_checked(&Bn254Fr::from_u64(*v));
        }

        let mut delta = Bn254Fr::new();
        for out_rho in out_rhos {
            let out_fr = bn254fr_from_hash32_be(out_rho);
            for in_rho in in_rhos {
                let in_fr = bn254fr_from_hash32_be(in_rho);
                submod_checked(&mut delta, &out_fr, &in_fr);
                enforce_prod.mulmod_checked(&delta);
            }
        }
        if out_rhos.len() == 2 {
            let a = bn254fr_from_hash32_be(&out_rhos[0]);
            let b = bn254fr_from_hash32_be(&out_rhos[1]);
            submod_checked(&mut delta, &a, &b);
            enforce_prod.mulmod_checked(&delta);
        }

        let mut inv = enforce_prod.clone();
        inv.inverse();
        inv.to_bytes_be()
    }

    let n_in: usize = inputs.len();
    let n_out: usize = if has_change { 2 } else { 1 };
    let withdraw_amount: u64 = 0;
    let withdraw_to: Hash32 = [0u8; 32];

    let mut out_values: Vec<u64> = vec![send_amount_u64];
    let mut out_rhos: Vec<Hash32> = vec![out_rho_0];
    if has_change {
        out_values.push(change_amount_u64);
        out_rhos.push(out_rho_1.expect("change rho set when has_change"));
    }
    let inv_enforce = inv_enforce_v2(&in_values_u64, &in_rhos, &out_values, &out_rhos);

    fn u64_to_i64(v: u64, label: &'static str) -> Result<i64> {
        i64::try_from(v).with_context(|| {
            format!("{label} does not fit into i64 (required by note_spend_guest v2 ABI)")
        })
    }

    fn arg32(b: &Hash32) -> LigeroProgramArguments {
        LigeroProgramArguments::HexBytesB64 {
            hex: hex::encode(b),
            bytes_b64: general_purpose::STANDARD.encode(b),
        }
    }

    // Build args + private indices in the exact order required by note_spend_guest v2.
    let mut private_indices: Vec<u32> = Vec::new();
    let mut proof_args: Vec<LigeroProgramArguments> = Vec::new();
    let push = |arg: LigeroProgramArguments,
                private: bool,
                private_indices: &mut Vec<u32>,
                proof_args: &mut Vec<LigeroProgramArguments>| {
        proof_args.push(arg);
        if private {
            private_indices.push(proof_args.len() as u32); // 1-based
        }
    };

    // Header:
    push(arg32(&DOMAIN), false, &mut private_indices, &mut proof_args); // 1 domain
    push(arg32(&spend_sk), true, &mut private_indices, &mut proof_args); // 2 spend_sk
    push(
        arg32(&pk_ivk_owner),
        true,
        &mut private_indices,
        &mut proof_args,
    ); // 3 pk_ivk_owner
    push(
        LigeroProgramArguments::I64 {
            i64: u64_to_i64(depth as u64, "depth")?,
        },
        false,
        &mut private_indices,
        &mut proof_args,
    ); // 4 depth
    push(arg32(&anchor_root), false, &mut private_indices, &mut proof_args); // 5 anchor
    push(
        LigeroProgramArguments::I64 {
            i64: u64_to_i64(n_in as u64, "n_in")?,
        },
        false,
        &mut private_indices,
        &mut proof_args,
    ); // 6 n_in

    // Inputs (0..n_in)
    for i in 0..n_in {
        // value_in_i [PRIVATE]
        push(
            LigeroProgramArguments::I64 {
                i64: u64_to_i64(in_values_u64[i], "value_in")?,
            },
            true,
            &mut private_indices,
            &mut proof_args,
        );
        // rho_in_i [PRIVATE]
        push(
            arg32(&in_rhos[i]),
            true,
            &mut private_indices,
            &mut proof_args,
        );
        // sender_id_in_i [PRIVATE]
        push(
            arg32(&in_sender_ids[i]),
            true,
            &mut private_indices,
            &mut proof_args,
        );
        // pos_i [PRIVATE]
        push(
            LigeroProgramArguments::I64 {
                i64: u64_to_i64(positions[i], "pos")?,
            },
            true,
            &mut private_indices,
            &mut proof_args,
        );
        // siblings_i[k] [PRIVATE]
        for s in &siblings_by_input[i] {
            push(arg32(s), true, &mut private_indices, &mut proof_args);
        }
        // nullifier_i [PUBLIC]
        push(
            arg32(&nullifiers[i]),
            false,
            &mut private_indices,
            &mut proof_args,
        );
    }

    // Withdraw binding.
    push(
        LigeroProgramArguments::I64 {
            i64: u64_to_i64(withdraw_amount, "withdraw_amount")?,
        },
        false,
        &mut private_indices,
        &mut proof_args,
    );
    push(
        arg32(&withdraw_to),
        false,
        &mut private_indices,
        &mut proof_args,
    );
    push(
        LigeroProgramArguments::I64 {
            i64: u64_to_i64(n_out as u64, "n_out")?,
        },
        false,
        &mut private_indices,
        &mut proof_args,
    );

    // Output 0.
    push(
        LigeroProgramArguments::I64 {
            i64: u64_to_i64(send_amount_u64, "value_out_0")?,
        },
        true,
        &mut private_indices,
        &mut proof_args,
    );
    push(
        arg32(&out_rho_0),
        true,
        &mut private_indices,
        &mut proof_args,
    );
    push(
        arg32(&destination_pk_spend),
        true,
        &mut private_indices,
        &mut proof_args,
    );
    push(
        arg32(&destination_pk_ivk),
        true,
        &mut private_indices,
        &mut proof_args,
    );
    push(
        arg32(&cm_out_0),
        false,
        &mut private_indices,
        &mut proof_args,
    );

    // Output 1 (change).
    if has_change {
        let rho1 = out_rho_1.expect("change rho set when has_change");
        let cm1 = cm_out_1.expect("change cm set when has_change");
        push(
            LigeroProgramArguments::I64 {
                i64: u64_to_i64(change_amount_u64, "value_out_1")?,
            },
            true,
            &mut private_indices,
            &mut proof_args,
        );
        push(
            arg32(&rho1),
            true,
            &mut private_indices,
            &mut proof_args,
        );
        push(
            arg32(&pk_spend_owner),
            true,
            &mut private_indices,
            &mut proof_args,
        );
        push(
            arg32(&pk_ivk_owner),
            true,
            &mut private_indices,
            &mut proof_args,
        );
        push(
            arg32(&cm1),
            false,
            &mut private_indices,
            &mut proof_args,
        );
    }

    // inv_enforce (private).
    push(
        arg32(&inv_enforce),
        true,
        &mut private_indices,
        &mut proof_args,
    );

    // === Deny-map (blacklist) arguments ===
    //
    // ABI extension (note_spend_guest v2 w/ deny-map buckets):
    //   - blacklist_root (PUBLIC)
    //   - for each checked id:
    //       bucket_entries[BLACKLIST_BUCKET_SIZE] (PRIVATE)
    //       bucket_inv (PRIVATE)
    //       bucket_siblings[BLACKLIST_TREE_DEPTH] (PRIVATE)
    //
    // Viewer arguments, if any, come AFTER this section.
    let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
    anyhow::ensure!(
        sender_opening.siblings.len() == bl_depth,
        "sender deny-map opening has wrong sibling length: got {}, expected {}",
        sender_opening.siblings.len(),
        bl_depth
    );
    anyhow::ensure!(
        dest_opening.siblings.len() == bl_depth,
        "destination deny-map opening has wrong sibling length: got {}, expected {}",
        dest_opening.siblings.len(),
        bl_depth
    );

    fn bl_bucket_inv_for_id(
        id: &Hash32,
        bucket_entries: &midnight_privacy::BlacklistBucketEntries,
    ) -> Result<Hash32> {
        let id_fr = bn254fr_from_hash32_be(id);
        let mut prod = Bn254Fr::from_u32(1);
        let mut delta = Bn254Fr::new();
        for e in bucket_entries.iter() {
            let e_fr = bn254fr_from_hash32_be(e);
            submod_checked(&mut delta, &id_fr, &e_fr);
            prod.mulmod_checked(&delta);
        }
        anyhow::ensure!(
            !prod.is_zero(),
            "deny-map bucket collision: id is present in bucket entries"
        );
        let mut inv = prod.clone();
        inv.inverse();
        Ok(inv.to_bytes_be())
    }

    push(arg32(&blacklist_root), false, &mut private_indices, &mut proof_args);

    // Opening 0: sender_id (spender identity)
    for e in sender_opening.bucket_entries.iter() {
        push(arg32(e), true, &mut private_indices, &mut proof_args);
    }
    let sender_inv = bl_bucket_inv_for_id(&sender_opening.recipient, &sender_opening.bucket_entries)?;
    push(arg32(&sender_inv), true, &mut private_indices, &mut proof_args);
    for sib in sender_opening.siblings.iter().take(bl_depth) {
        push(arg32(sib), true, &mut private_indices, &mut proof_args);
    }

    // Opening 1: pay recipient (transfer only; change outputs are enforced to be self in-circuit).
    for e in dest_opening.bucket_entries.iter() {
        push(arg32(e), true, &mut private_indices, &mut proof_args);
    }
    let dest_inv = bl_bucket_inv_for_id(&dest_opening.recipient, &dest_opening.bucket_entries)?;
    push(arg32(&dest_inv), true, &mut private_indices, &mut proof_args);
    for sib in dest_opening.siblings.iter().take(bl_depth) {
        push(arg32(sib), true, &mut private_indices, &mut proof_args);
    }

    // Viewer section arguments (Level B) if viewer FVK is configured.
    let viewer_fvk_commitment_arg_idx: Option<usize> =
        if let (Some(ref bundle), Some(ref atts)) = (viewer_fvk_bundle.as_ref(), &view_attestations)
        {
        // n_viewers
        push(
            LigeroProgramArguments::I64 { i64: 1 },
            false,
            &mut private_indices,
            &mut proof_args,
        );
        let fvk_commitment_arg_idx = proof_args.len();
        // fvk_commitment (public)
        push(
            arg32(&bundle.fvk_commitment),
            false,
            &mut private_indices,
            &mut proof_args,
        );
        // fvk (private)
        push(arg32(&bundle.fvk), true, &mut private_indices, &mut proof_args);
        // For each output, ct_hash + mac (public)
        for att in atts.iter().take(n_out) {
            push(arg32(&att.ct_hash), false, &mut private_indices, &mut proof_args);
            push(arg32(&att.mac), false, &mut private_indices, &mut proof_args);
        }
        Some(fvk_commitment_arg_idx)
    } else {
        None
    };

    // Save args/private indices for packaging (verifier expects a LigeroProofPackage)
    let proof_args_for_package = proof_args.clone();
    let private_indices_for_package: Vec<usize> =
        private_indices.iter().map(|i| *i as usize).collect();

    let proof_start = StdInstant::now();
    let proof_bytes_raw = ligero
        .generate_proof(private_indices, proof_args)
        .await
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
        blacklist_root,
        nullifiers: nullifiers.clone(),
        withdraw_amount: 0, // pure shielded transfer, no transparent withdrawal
        output_commitments,
        view_attestations,
    };

    let mut args_json_values: Vec<serde_json::Value> = proof_args_for_package
        .iter()
        .map(|a| serde_json::to_value(a))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to serialize Ligero args to JSON values for package")?;

    if let (Some(idx), Some(ref bundle)) = (viewer_fvk_commitment_arg_idx, viewer_fvk_bundle.as_ref()) {
        let obj = args_json_values[idx].as_object_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "viewer.fvk_commitment arg must serialize to a JSON object to attach pool_sig_hex"
            )
        })?;
        obj.insert(
            "pool_sig_hex".to_string(),
            serde_json::Value::String(bundle.pool_sig_hex.clone()),
        );
    }

    let args_json = serde_json::to_vec(&args_json_values)
        .context("Failed to serialize Ligero args for package")?;
    let proof_package = LigeroProofPackage::new(
        proof_bytes_raw,
        bincode::serialize(&public_output).context("Failed to serialize spend public output")?,
        args_json,
        private_indices_for_package,
    )
    .context("Failed to build LigeroProofPackage")?;

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
        nullifiers,
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
    let submit_result = provider
        .submit_to_verifier(raw_tx)
        .await
        .context("Failed to submit transaction to verifier service")?;
    let tx_hash = submit_result.tx_hash;

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
        created_at: submit_result.created_at,
        amount_sent: send_amount,
        output_rho: out_rho_0,
        output_recipient: out_recipient_0,
        change_amount: if has_change {
            Some(change_amount)
        } else {
            None
        },
        change_rho: out_rho_1,
        change_recipient: out_recipient_1,
    })
}
