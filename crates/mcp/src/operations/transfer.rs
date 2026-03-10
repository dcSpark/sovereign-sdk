//! Transfer operation for Midnight Privacy module

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use midnight_privacy::{
    inv_enforce_v2, nf_key_from_sk, note_commitment, nullifier, pk_from_sk, recipient_from_pk_v2,
    recipient_from_sk_v2, CallMessage as MidnightCallMessage, EncryptedNote, Hash32, MerkleTree,
    PrivacyAddress, SpendPublic,
};
use serde::Deserialize;
use sov_address::MultiAddressEvm;
use sov_api_spec::types as api_types;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;
use sov_nightstream_adapter::Nightstream as NightstreamAdapter;
use sov_nightstream_adapter::{
    BlacklistProof, NoteSpendInput, NoteSpendOutput, NoteSpendWitness, ViewerOutputWitness,
    ViewerWitness,
};
use std::time::{Duration, Instant as StdInstant};
use tokio::time::{sleep, Instant as TokioInstant};

use crate::fvk_service::ViewerFvkBundle;
use crate::nightstream::Nightstream;
use crate::provider::Provider;
use crate::viewer;
use crate::wallet::WalletContext;

pub type McpSpec =
    ConfigurableSpec<MockDaSpec, NightstreamAdapter, MockZkvm, MultiAddressEvm, Native>;
pub type McpRuntime = Runtime<McpSpec>;

const TREE_DEPTH: u8 = 16;
const DOMAIN: [u8; 32] = [1u8; 32];
const INCLUSION_POLL_INTERVAL_MS: u64 = 100;
const INCLUSION_TIMEOUT_SECS: u64 = 60;
const INCLUSION_LOG_INTERVAL_SECS: u64 = 5;
const MERKLE_FETCH_LOG_EVERY: usize = 10;
const _NOTE_SEARCH_LOG_EVERY: usize = 10;

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
            }

            offset += batch_resp.notes.len();
        }
    }

    let mut root = [0u8; 32];
    root.copy_from_slice(&state.root);

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
    let mut _scanned = 0usize;

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
        _scanned += len;

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

        if len < batch_size {
            break;
        }
        offset += batch_size;
    }

    Ok(None)
}

/// Get a recent valid anchor root
async fn get_anchor_root(provider: &Provider) -> Result<Hash32> {
    let roots_state: RootsResp = provider
        .query_rest_endpoint("/modules/midnight-privacy/roots/recent")
        .await
        .context("Failed to query recent roots")?;

    let anchor = roots_state
        .recent_roots
        .last()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("No recent roots available"))?;

    Ok(anchor)
}

/// Poll the ledger for transaction inclusion
async fn wait_for_inclusion(provider: &Provider, tx_hash: &str) -> Result<()> {
    let start = TokioInstant::now();
    let deadline = start + Duration::from_secs(INCLUSION_TIMEOUT_SECS);
    let mut last_log = start;
    let mut _attempts: u64 = 0;

    loop {
        _attempts += 1;
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
                return Ok(());
            }
            Err(_) => {
                if last_log.elapsed() >= Duration::from_secs(INCLUSION_LOG_INTERVAL_SECS) {
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
/// Uses Nightstream prover service: builds SpendPublic and delegates proof generation.
pub async fn transfer(
    nightstream: &Nightstream,
    provider: &Provider,
    wallet: &WalletContext<McpRuntime, McpSpec>,
    spend_sk: Hash32,
    pk_ivk_owner: Hash32,
    note_value: u128,
    send_amount: u128,
    input_rho: [u8; 32],
    input_sender_id: Hash32,
    destination_pk_spend: Hash32,
    destination_pk_ivk: Hash32,
    viewer_fvk_bundle: Option<ViewerFvkBundle>,
) -> Result<TransferResult> {
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
    let change_amount = if has_change {
        note_value - send_amount
    } else {
        0
    };
    let note_value_u64: u64 = note_value
        .try_into()
        .context("note_value does not fit into u64")?;
    let send_amount_u64: u64 = send_amount
        .try_into()
        .context("send_amount does not fit into u64")?;
    let change_amount_u64: u64 = change_amount
        .try_into()
        .context("change_amount does not fit into u64")?;

    let input_recipient = recipient_from_sk_v2(&DOMAIN, &spend_sk, &pk_ivk_owner);
    let sender_id_out = input_recipient;
    let pk_spend_owner = pk_from_sk(&spend_sk);

    let (tree, _current_root) = fetch_merkle_tree(provider).await?;

    let input_cm = note_commitment(
        &DOMAIN,
        note_value_u64,
        &input_rho,
        &input_recipient,
        &input_sender_id,
    );

    let position = find_note_position(provider, input_cm)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Input note not found in tree. Searched for commitment: {}",
                hex::encode(&input_cm)
            )
        })?;

    let anchor_root = get_anchor_root(provider).await?;

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

    let (out_rho_1, out_recipient_1, cm_out_1) = if has_change {
        let rho: [u8; 32] = rand::random();
        let recipient = recipient_from_pk_v2(&DOMAIN, &pk_spend_owner, &pk_ivk_owner);
        let cm = note_commitment(&DOMAIN, change_amount_u64, &rho, &recipient, &sender_id_out);
        (Some(rho), Some(recipient), Some(cm))
    } else {
        (None, None, None)
    };

    let num_outputs: u32 = if has_change { 2 } else { 1 };

    let nf_key = nf_key_from_sk(&DOMAIN, &spend_sk);
    let nf = nullifier(&DOMAIN, &nf_key, &input_rho);

    let (view_attestations, view_ciphertexts) = if let Some(ref bundle) = viewer_fvk_bundle {
        let fvk = bundle.fvk;
        let mut cm_ins: [Hash32; viewer::MAX_INS] = [[0u8; 32]; viewer::MAX_INS];
        cm_ins[0] = input_cm;
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
        (None, None)
    };

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
        "Deny-map root changed while fetching openings"
    );
    let blacklist_root = sender_opening.blacklist_root;

    if sender_opening.is_blacklisted {
        anyhow::bail!("Sender privacy address is frozen (blacklisted)");
    }
    if dest_opening.is_blacklisted {
        anyhow::bail!("Destination privacy address is frozen (blacklisted)");
    }

    // Build NoteSpendWitness for the prover service
    let siblings = tree.open(position as usize);

    let witness_inputs = vec![NoteSpendInput {
        value: note_value_u64,
        rho: input_rho,
        sender_id: input_sender_id,
        position: position as u32,
        siblings,
        nullifier: nf,
    }];

    let mut witness_outputs = vec![NoteSpendOutput {
        value: send_amount_u64,
        rho: out_rho_0,
        pk_spend: destination_pk_spend,
        pk_ivk: destination_pk_ivk,
        cm: cm_out_0,
    }];
    if has_change {
        witness_outputs.push(NoteSpendOutput {
            value: change_amount_u64,
            rho: out_rho_1.unwrap(),
            pk_spend: pk_spend_owner,
            pk_ivk: pk_ivk_owner,
            cm: cm_out_1.unwrap(),
        });
    }

    let mut out_values_for_inv = vec![send_amount_u64];
    let mut out_rhos_for_inv = vec![out_rho_0];
    if has_change {
        out_values_for_inv.push(change_amount_u64);
        out_rhos_for_inv.push(out_rho_1.unwrap());
    }
    let inv_enforce = inv_enforce_v2(
        &[note_value_u64],
        &[input_rho],
        &out_values_for_inv,
        &out_rhos_for_inv,
    );

    let mut blacklist_proofs = vec![BlacklistProof::from_opening(
        &sender_opening.recipient,
        sender_opening.bucket_entries,
        sender_opening.siblings.clone(),
    )];
    blacklist_proofs.push(BlacklistProof::from_opening(
        &dest_opening.recipient,
        dest_opening.bucket_entries,
        dest_opening.siblings.clone(),
    ));

    let viewer_witnesses: Vec<ViewerWitness> = if let Some(ref atts) = view_attestations {
        if let Some(ref bundle) = viewer_fvk_bundle {
            vec![ViewerWitness {
                fvk_commitment: atts[0].fvk_commitment,
                fvk: bundle.fvk,
                per_output: atts
                    .iter()
                    .map(|a| ViewerOutputWitness {
                        ct_hash: a.ct_hash,
                        mac: a.mac,
                    })
                    .collect(),
            }]
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let depth = witness_inputs[0].siblings.len();
    let witness = NoteSpendWitness {
        domain: DOMAIN,
        spend_sk,
        pk_ivk_owner,
        depth: depth as u32,
        anchor: anchor_root,
        inputs: witness_inputs,
        withdraw_amount: 0,
        withdraw_to: [0u8; 32],
        outputs: witness_outputs,
        inv_enforce,
        blacklist_root,
        blacklist_proofs,
        viewers: viewer_witnesses,
    };

    let mut output_commitments = vec![cm_out_0];
    if has_change {
        output_commitments.push(cm_out_1.unwrap());
    }
    let public = SpendPublic {
        anchor_root,
        blacklist_root,
        nullifiers: vec![nf],
        withdraw_amount: 0,
        output_commitments,
        view_attestations,
    };

    tracing::info!(
        "Generating Nightstream ZK proof with {} output(s)...",
        num_outputs
    );

    let proof_bytes = nightstream
        .generate_proof(&witness, &public)
        .await
        .inspect_err(|e| {
            tracing::error!("Failed to generate Nightstream proof for transfer: {:?}", e)
        })
        .context("Failed to generate Nightstream proof for transfer")?;

    let proof_bytes = if let Some(ref bundle) = viewer_fvk_bundle {
        tracing::info!("Injecting pool viewer signature into proof package");
        crate::nightstream::inject_pool_viewer_sig(
            proof_bytes,
            bundle.fvk_commitment,
            &bundle.pool_sig_hex,
        )
        .context("Failed to inject pool viewer signature into proof package")?
    } else {
        proof_bytes
    };

    let unsigned_tx = create_transfer_unsigned_tx(
        provider,
        wallet,
        proof_bytes,
        anchor_root,
        vec![nf],
        view_ciphertexts,
    )
    .await?;

    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .context("Failed to sign transaction")?;

    let tx_hash = provider
        .submit_to_verifier(raw_tx)
        .await
        .context("Failed to submit transaction to verifier service")?;

    wait_for_inclusion(provider, &tx_hash).await?;

    Ok(TransferResult {
        tx_hash,
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
