//! Transfer operation for Midnight Privacy module

use anyhow::{Context, Result};
use demo_stf::runtime::Runtime;
use midnight_privacy::{
    inv_enforce_v2, nf_key_from_sk, note_commitment, nullifier, pk_from_sk, recipient_from_pk_v2,
    recipient_from_sk_v2, CallMessage as MidnightCallMessage, EncryptedNote, Hash32,
    PrivacyAddress, SpendPublic,
};
use sov_address::MultiAddressEvm;
use sov_api_spec::types as api_types;
use sov_mock_da::MockDaSpec;
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::configurable_spec::ConfigurableSpec;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::Amount;
use sov_nightstream_adapter::{
    BlacklistProof, NoteSpendInput, NoteSpendOutput, NoteSpendWitness, ViewerOutputWitness,
    ViewerWitness,
};
use std::time::{Duration, Instant as StdInstant};
use tokio::time::{sleep, Instant as TokioInstant};

use crate::commitment_tree::global_tree_syncer;
use crate::fvk_service::ViewerFvkBundle;
use crate::nightstream::Nightstream;
use crate::operations::DEFAULT_MAX_FEE;
use crate::provider::Provider;
use crate::viewer;
use crate::wallet::WalletContext;

pub type McpSpec = ConfigurableSpec<
    MockDaSpec,
    sov_nightstream_adapter::Nightstream,
    MockZkvm,
    MultiAddressEvm,
    Native,
>;
pub type McpRuntime = Runtime<McpSpec>;

const DOMAIN: [u8; 32] = [1u8; 32];
const SEQUENCER_CONFIRM_POLL_INTERVAL_MS: u64 = 100;
const SEQUENCER_CONFIRM_TIMEOUT_SECS: u64 = 10;
const SEQUENCER_CONFIRM_LOG_INTERVAL_SECS: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferConfirmation {
    /// No confirmation wait was performed.
    Skipped,
    /// The sequencer returned a successful receipt.
    SequencerConfirmed,
    /// The sequencer did not confirm within the configured timeout.
    PendingTimeout,
}

impl TransferConfirmation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::SequencerConfirmed => "sequencer_confirmed",
            Self::PendingTimeout => "pending_timeout",
        }
    }
}

#[derive(Debug)]
pub struct TransferResult {
    pub tx_hash: String,
    pub created_at: i64,
    pub confirmation: TransferConfirmation,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferWaitMode {
    None,
    Sequencer,
}

impl TransferWaitMode {
    fn from_env() -> Self {
        let raw =
            std::env::var("MCP_TRANSFER_WAIT_MODE").unwrap_or_else(|_| "sequencer".to_string());
        let v = raw.trim().to_ascii_lowercase();
        match v.as_str() {
            "" | "sequencer" | "seq" => Self::Sequencer,
            "none" | "off" | "false" | "0" => Self::None,
            other => {
                tracing::warn!(
                    "Unknown MCP_TRANSFER_WAIT_MODE '{}'; falling back to 'sequencer'",
                    other
                );
                Self::Sequencer
            }
        }
    }
}

/// Poll the sequencer for a soft confirmation.
async fn wait_for_sequencer_confirmation(provider: &Provider, tx_hash: &str) -> Result<bool> {
    let start = TokioInstant::now();
    let deadline = start + Duration::from_secs(SEQUENCER_CONFIRM_TIMEOUT_SECS);
    let mut last_log = start;
    let mut attempts: u64 = 0;

    loop {
        attempts += 1;
        if TokioInstant::now() > deadline {
            tracing::warn!(
                tx_hash,
                attempts,
                waited_ms = start.elapsed().as_millis(),
                "Timed out waiting for sequencer confirmation; returning tx hash as pending"
            );
            return Ok(false);
        }

        match provider.get_sequencer_tx(tx_hash).await {
            Ok(Some(tx)) => match tx.receipt.result {
                api_types::TxReceiptResult::Successful => {
                    tracing::debug!(
                        tx_hash,
                        attempts,
                        waited_ms = start.elapsed().as_millis(),
                        "Transaction confirmed by sequencer"
                    );
                    return Ok(true);
                }
                api_types::TxReceiptResult::Reverted | api_types::TxReceiptResult::Skipped => {
                    anyhow::bail!(
                        "Transaction {} confirmed by sequencer but not successful: {:?}",
                        tx_hash,
                        tx.receipt
                    );
                }
            },
            Ok(None) => {}
            Err(err) => {
                if last_log.elapsed() >= Duration::from_secs(SEQUENCER_CONFIRM_LOG_INTERVAL_SECS) {
                    tracing::debug!(
                        tx_hash,
                        attempts,
                        error = %err,
                        "Latest sequencer confirmation check failed"
                    );
                }
            }
        }

        if last_log.elapsed() >= Duration::from_secs(SEQUENCER_CONFIRM_LOG_INTERVAL_SECS) {
            let remaining_secs = deadline
                .checked_duration_since(TokioInstant::now())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            tracing::info!(
                tx_hash,
                attempts,
                waited_ms = start.elapsed().as_millis(),
                remaining_secs,
                "Waiting for sequencer confirmation"
            );
            last_log = TokioInstant::now();
        }

        sleep(Duration::from_millis(SEQUENCER_CONFIRM_POLL_INTERVAL_MS)).await;
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
    nightstream: &Nightstream,
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

    tracing::debug!(
        "Starting transfer: n_in={}, sum_in={}, send_amount={}, change_amount={}",
        inputs.len(),
        sum_in_u64,
        send_amount,
        change_amount
    );
    let overall_start = StdInstant::now();

    // Timing breakdown for instrumentation
    let timing_tree_ms: u128;
    let timing_proof_ms: u128;
    let timing_tx_build_ms: u128;
    let timing_sign_ms: u128;
    let timing_submit_ms: u128;
    let timing_confirm_ms: u128;

    // Derive the spender's privacy recipient (owner address) from (spend_sk, pk_ivk_owner).
    // This matches note_spend_guest v2, where the input recipient is derived in-circuit.
    let input_recipient = recipient_from_sk_v2(&DOMAIN, &spend_sk, &pk_ivk_owner);
    let sender_id_out = input_recipient;
    let pk_spend_owner = pk_from_sk(&spend_sk);

    // Step 1: Compute input commitments and resolve positions + auth paths using a shared,
    // incrementally-synced Merkle tree cache.
    let mut input_cms: Vec<Hash32> = Vec::with_capacity(inputs.len());
    for i in 0..inputs.len() {
        input_cms.push(note_commitment(
            &DOMAIN,
            in_values_u64[i],
            &in_rhos[i],
            &input_recipient,
            &in_sender_ids[i],
        ));
    }

    let tree_start = StdInstant::now();
    let (anchor_root, positions, siblings_by_input) = global_tree_syncer()
        .resolve_positions_and_openings(provider, &input_cms)
        .await
        .context("Failed to resolve Merkle positions/openings from cached commitment tree")?;
    timing_tree_ms = tree_start.elapsed().as_millis();
    tracing::debug!(
        elapsed_ms = timing_tree_ms,
        anchor_root = %hex::encode(anchor_root),
        "Merkle tree sync completed"
    );

    let depth = siblings_by_input.first().map(|s| s.len()).unwrap_or(0);
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
        tracing::debug!(
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

    // Step 5: Build NoteSpendWitness, SpendPublic, and generate Nightstream proof
    let witness_inputs: Vec<NoteSpendInput> = (0..in_values_u64.len())
        .map(|i| NoteSpendInput {
            value: in_values_u64[i],
            rho: in_rhos[i],
            sender_id: in_sender_ids[i],
            position: positions[i] as u32,
            siblings: siblings_by_input[i].clone(),
            nullifier: nullifiers[i],
        })
        .collect();

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
        &in_values_u64,
        &in_rhos,
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
        nullifiers: nullifiers.clone(),
        withdraw_amount: 0,
        output_commitments,
        view_attestations,
    };

    tracing::debug!(
        "Generating Nightstream ZK proof with {} output(s)...",
        num_outputs
    );
    let proof_start = StdInstant::now();
    let pool_viewer_signature = viewer_fvk_bundle.as_ref().map(|bundle| {
        crate::nightstream::PoolViewerSignature {
            fvk_commitment: bundle.fvk_commitment,
            pool_sig_hex: bundle.pool_sig_hex.clone(),
        }
    });
    let generated_proof = nightstream
        .generate_proof(&witness, &public, pool_viewer_signature.as_ref())
        .await
        .inspect_err(|e| {
            tracing::error!("Failed to generate Nightstream proof for transfer: {:?}", e)
        })
        .context("Failed to generate Nightstream proof for transfer")?;
    timing_proof_ms = proof_start.elapsed().as_millis();

    tracing::debug!(
        elapsed_ms = timing_proof_ms,
        proof_bytes_len = generated_proof.proof_bytes.len(),
        "Generated Nightstream proof"
    );
    let proof_bytes = generated_proof.proof_bytes;

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
    timing_tx_build_ms = unsigned_tx_start.elapsed().as_millis();
    tracing::debug!(
        elapsed_ms = timing_tx_build_ms,
        "Unsigned transfer transaction created"
    );

    let sign_start = StdInstant::now();
    let raw_tx = wallet
        .sign_transaction::<McpRuntime>(unsigned_tx)
        .context("Failed to sign transaction")?;
    timing_sign_ms = sign_start.elapsed().as_millis();

    tracing::debug!(
        elapsed_ms = timing_sign_ms,
        tx_bytes_len = raw_tx.len(),
        "Transaction signed"
    );

    // Step 7: Submit transaction to verifier service
    let submit_start = StdInstant::now();
    let submit_result = provider
        .submit_to_verifier(raw_tx, generated_proof.proof_ref.as_ref())
        .await
        .context("Failed to submit transaction to verifier service")?;
    let tx_hash = submit_result.tx_hash;
    timing_submit_ms = submit_start.elapsed().as_millis();

    tracing::debug!(
        elapsed_ms = timing_submit_ms,
        tx_hash,
        "Transfer transaction submitted via verifier service"
    );

    // Step 8: Optional post-submit wait
    let confirm_start = StdInstant::now();
    let confirmation = match TransferWaitMode::from_env() {
        TransferWaitMode::None => {
            tracing::debug!("Skipping post-submit wait (MCP_TRANSFER_WAIT_MODE=none)");
            TransferConfirmation::Skipped
        }
        TransferWaitMode::Sequencer => {
            tracing::debug!("Waiting for sequencer confirmation...");
            let confirmed = wait_for_sequencer_confirmation(provider, &tx_hash).await?;
            if confirmed {
                TransferConfirmation::SequencerConfirmed
            } else {
                TransferConfirmation::PendingTimeout
            }
        }
    };
    timing_confirm_ms = confirm_start.elapsed().as_millis();

    let total_ms = overall_start.elapsed().as_millis();
    tracing::info!(
        tx_hash,
        total_ms,
        tree_ms = timing_tree_ms,
        proof_ms = timing_proof_ms,
        tx_build_ms = timing_tx_build_ms,
        sign_ms = timing_sign_ms,
        submit_ms = timing_submit_ms,
        confirm_ms = timing_confirm_ms,
        confirmation = confirmation.as_str(),
        "[TRANSFER_TIMING] Transfer completed"
    );

    Ok(TransferResult {
        tx_hash,
        created_at: submit_result.created_at,
        confirmation,
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
