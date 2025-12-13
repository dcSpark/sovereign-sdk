//! Shared utilities for E2E benchmark runners.
//!
//! This module contains common code used by both `e2e_runner` and `continuous_transfers`,
//! including constants, type aliases, helper functions for chain interaction, and
//! proof generation utilities.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use midnight_privacy::{
    note_commitment, nullifier, Hash32, SpendPublic, EncryptedNote,
    RecipientAttestation, Note,
    viewing::{encrypt_note_for_recipient_with_sender, ct_hash as compute_ct_hash},
    pk_from_sk, pk_ivk_from_sk, recipient_from_pk, nf_key_from_sk,
};
use serde::Deserialize;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_node_client::NodeClient;
use sov_rollup_ligero::MockDemoRollup;

use crate::make_viewer_bundle;

// =============================================================================
// Type Aliases
// =============================================================================

/// Match the spec used by the demo rollup binary.
pub type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

// =============================================================================
// Constants
// =============================================================================

/// Tree depth - must match ValueSetterZkConfig in genesis (demo/mock/midnight_privacy.json)
pub const TREE_DEPTH: u8 = 16;

/// Domain constant - must match genesis config
pub const DOMAIN: Hash32 = [1u8; 32];

/// Initial deposit amount for benchmark wallets
pub const INITIAL_DEPOSIT_AMOUNT: u128 = 100;

/// Maximum retries for tree rebuild operations
pub const TREE_REBUILD_MAX_RETRIES: usize = 5;

/// Delay between tree rebuild retries (ms)
pub const TREE_REBUILD_RETRY_DELAY_MS: u64 = 500;

/// Maximum retries when looking for missing notes
pub const MISSING_NOTE_RETRY_MAX: usize = 10;

/// Delay between missing note retries (ms)
pub const MISSING_NOTE_RETRY_DELAY_MS: u64 = 300;

// =============================================================================
// Path Helpers
// =============================================================================

/// Get the path to the rollup-ligero example crate directory.
pub fn rollup_crate_dir() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("examples/rollup-ligero").exists())
        .ok_or_else(|| anyhow!("Could not find repository root"))?;
    Ok(repo_root.join("examples/rollup-ligero"))
}

/// Get the path to the genesis keypairs file.
pub fn genesis_keypairs_path(crate_dir: &Path) -> PathBuf {
    crate_dir
        .parent()
        .expect("examples/ directory should have a parent")
        .join("test-data/genesis/demo/mock/generated_keypairs.json")
}

/// Load pre-generated genesis keypairs from disk.
pub fn load_demo_genesis_keypairs(
    crate_dir: &Path,
) -> Result<Vec<PrivateKeyAndAddress<DemoRollupSpec>>> {
    let keypairs_path = genesis_keypairs_path(crate_dir);
    if !keypairs_path.exists() {
        bail!(
            "Generated keypairs file not found at {}. Please run: cargo run --bin generate-genesis-keys",
            keypairs_path.display()
        );
    }

    let keypairs_json = std::fs::read_to_string(&keypairs_path).with_context(|| {
        format!(
            "Failed to read keypairs file at {}",
            keypairs_path.display()
        )
    })?;

    let all_keypairs: Vec<PrivateKeyAndAddress<DemoRollupSpec>> =
        serde_json::from_str(&keypairs_json).with_context(|| {
            format!(
                "Failed to parse keypairs file at {}",
                keypairs_path.display()
            )
        })?;

    Ok(all_keypairs)
}

// =============================================================================
// Chain Hash Fetching
// =============================================================================

#[derive(Deserialize)]
struct SchemaResp {
    chain_hash: String,
}

/// Fetch the authoritative chain_hash from the node's `/rollup/schema` endpoint.
pub async fn fetch_chain_hash(client: &NodeClient) -> Result<[u8; 32]> {
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
    Ok(chain_hash)
}

// =============================================================================
// Note Fetching
// =============================================================================

/// Information about a single note in the tree.
#[derive(Deserialize, Clone)]
pub struct NoteInfo {
    pub position: u64,
    pub commitment: Vec<u8>,
}

/// Response from the notes endpoint.
#[derive(Deserialize, Clone)]
pub struct NotesResp {
    pub notes: Vec<NoteInfo>,
    #[serde(default)]
    pub current_root: Option<Vec<u8>>,
    #[serde(default)]
    #[allow(dead_code)]
    pub count: Option<u64>,
}

/// Fetch all notes from the node using pagination.
pub async fn fetch_all_notes(client: &NodeClient) -> Result<Vec<NoteInfo>> {
    let batch_size = 1000usize;
    let mut offset = 0usize;
    let mut all_notes = Vec::new();

    loop {
        let endpoint = format!(
            "/modules/midnight-privacy/notes?limit={}&offset={}",
            batch_size, offset
        );
        let batch_resp: NotesResp = client
            .query_rest_endpoint(&endpoint)
            .await
            .with_context(|| format!("Failed to query notes batch at offset {}", offset))?;

        let batch_len = batch_resp.notes.len();
        all_notes.extend(batch_resp.notes);

        if batch_len < batch_size {
            break;
        }
        offset += batch_size;
    }

    Ok(all_notes)
}

/// Fetch note positions and build a commitment -> position map.
pub async fn fetch_note_positions(client: &NodeClient) -> Result<HashMap<Hash32, u64>> {
    let mut pos_by_cm: HashMap<Hash32, u64> = HashMap::new();
    for n in fetch_all_notes(client).await? {
        if n.commitment.len() == 32 {
            let mut cm = [0u8; 32];
            cm.copy_from_slice(&n.commitment);
            pos_by_cm.insert(cm, n.position);
        }
    }
    Ok(pos_by_cm)
}

// =============================================================================
// Verifier Communication
// =============================================================================

/// Metrics returned by the proof verifier service.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VerifierMetrics {
    #[serde(default)]
    pub deserialize_ms: f64,
    #[serde(default)]
    pub parse_ms: f64,
    #[serde(default)]
    pub signature_verify_ms: f64,
    #[serde(default)]
    pub proof_verify_ms: f64,
    #[serde(default)]
    pub tx_creation_ms: f64,
    #[serde(default)]
    pub node_submit_ms: f64,
    #[serde(default)]
    pub total_ms: f64,
}

/// Response from submitting a transaction to the verifier.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifierSubmitResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub sequencer_response: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub metrics: Option<VerifierMetrics>,
}

/// Submit to verifier and retry when Syncing is reported.
pub async fn submit_to_verifier_with_sync_retry(
    client: &reqwest::Client,
    verifier_url: &str,
    body_b64: &str,
    label: &str,
    idx: usize,
    timeout: Duration,
) -> Result<(VerifierSubmitResponse, f64)> {
    let mut backoff = Duration::from_millis(50);
    let max_backoff = Duration::from_secs(2);
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let submit_start = std::time::Instant::now();
        let resp = client
            .post(format!(
                "{}/midnight-privacy",
                verifier_url.trim_end_matches('/')
            ))
            .json(&serde_json::json!({
                "body": body_b64,
                "serialized_tx_base64": body_b64,
            }))
            .send()
            .await
            .with_context(|| format!("{} #{} request failed", label, idx))?;

        let http_elapsed_ms = submit_start.elapsed().as_secs_f64() * 1000.0;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .with_context(|| format!("{} #{} failed to read response body", label, idx))?;

        if status.as_u16() == 503 && text.contains("Syncing") {
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{} #{} retried but node still Syncing (HTTP 503): {}",
                    label,
                    idx,
                    text
                );
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
            continue;
        }

        let parsed: VerifierSubmitResponse = serde_json::from_str(&text)
            .with_context(|| format!("{} #{} invalid JSON: {}", label, idx, text))?;

        if !parsed.success {
            let syncing = parsed
                .error
                .as_deref()
                .map(|e| e.contains("\"Syncing\"") || e.contains("fell out of sync"))
                .unwrap_or(false);

            if syncing && std::time::Instant::now() < deadline {
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
                continue;
            }
        }

        return Ok((parsed, http_elapsed_ms));
    }
}

/// Flush queued transactions from verifier to sequencer.
pub async fn flush_verifier_queue(http: &reqwest::Client, verifier_url: &str) -> Result<String> {
    let resp = http
        .post(format!(
            "{}/midnight-privacy/flush",
            verifier_url.trim_end_matches('/')
        ))
        .send()
        .await
        .context("flush request failed")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("flush endpoint returned {}: {}", status, body);
    }
    Ok(body)
}

// =============================================================================
// Tree State
// =============================================================================

/// State of the Merkle tree from the node.
#[derive(Deserialize, Clone)]
pub struct TreeState {
    pub root: Vec<u8>,
    pub next_position: u64,
    #[serde(default)]
    #[allow(dead_code)]
    pub depth: Option<u8>,
}

/// Module statistics from the node.
#[derive(Deserialize, Clone, Copy, Debug, Default)]
pub struct ModuleStats {
    #[serde(default)]
    pub deposit_count: u64,
    #[serde(default)]
    pub nullifiers_spent: u64,
}

/// Recent Merkle roots response.
#[derive(Deserialize, Clone)]
pub struct RootsResp {
    pub recent_roots: Vec<Hash32>,
}

// =============================================================================
// Proof Generation (shared ABI for 1-in/1-out transfer)
// =============================================================================

/// Data needed to generate a transfer proof.
pub struct TransferProofInput {
    /// Spending secret key (derives all other keys)
    pub spend_sk: Hash32,
    /// Input note value
    pub value: u128,
    /// Input note rho
    pub in_rho: Hash32,
    /// Input note position in tree
    pub position: u64,
    /// Merkle siblings for auth path
    pub siblings: Vec<Hash32>,
    /// Anchor root for the proof
    pub anchor: Hash32,
    /// Output note rho
    pub out_rho: Hash32,
    /// Authority viewing key (optional, for Level-B compliance)
    pub authority_fvk: Option<Hash32>,
}

/// Result of proof generation containing data needed for tx building.
pub struct TransferProofOutput {
    /// The generated proof bytes
    pub proof_bytes: Vec<u8>,
    /// Nullifier for the input note
    pub nullifier: Hash32,
    /// Output commitment
    pub cm_out: Hash32,
    /// Output recipient address
    pub out_recipient: Hash32,
    /// Output pk_spend
    pub out_pk_spend: Hash32,
    /// Output pk_ivk
    pub out_pk_ivk: Hash32,
    /// Recipient ciphertext epk
    pub out_epk: Hash32,
    /// Recipient ciphertext hash
    pub out_ct_hash: Hash32,
    /// Recipient ciphertext MAC
    pub out_mac: Hash32,
}

/// Generate a 1-in/1-out transfer proof using the correct circuit ABI.
/// This is a self-transfer (output goes to same spend_sk owner).
pub fn generate_transfer_proof(
    program_path: &String,
    input: &TransferProofInput,
) -> Result<TransferProofOutput> {
    use sov_rollup_interface::zk::{Zkvm, ZkvmHost};

    let depth_usize = input.siblings.len();
    anyhow::ensure!(
        depth_usize == TREE_DEPTH as usize,
        "Expected {} siblings, got {}",
        TREE_DEPTH,
        depth_usize
    );

    // Derive keys from spend_sk
    let nf_key = nf_key_from_sk(&DOMAIN, &input.spend_sk);
    let nf = nullifier(&DOMAIN, &nf_key, &input.in_rho);

    // Output keys (self-transfer: same spend_sk)
    let out_pk_spend = pk_from_sk(&input.spend_sk);
    let out_pk_ivk = pk_ivk_from_sk(&DOMAIN, &input.spend_sk);
    let out_recipient = recipient_from_pk(&DOMAIN, &out_pk_spend, &out_pk_ivk);

    // Compute output commitment
    let cm_out = note_commitment(&DOMAIN, input.value, &input.out_rho, &out_recipient);

    // Sender ID (self-transfer: sender = recipient)
    let sender_id = recipient_from_pk(&DOMAIN, &out_pk_spend, &out_pk_ivk);

    // Build recipient ciphertext binding data (sender-bound)
    let out_note = Note {
        domain: DOMAIN,
        value: input.value,
        rho: input.out_rho,
        recipient: out_recipient,
    };
    let rec_ct = encrypt_note_for_recipient_with_sender(
        &DOMAIN, &out_pk_ivk, &out_note, &sender_id, &cm_out,
    )
    .context("Failed to encrypt note for recipient")?;
    let out_epk = rec_ct.epk;
    let out_ct_hash = compute_ct_hash(&rec_ct.ct);
    let out_mac = rec_ct.mac;

    // Build recipient attestation
    let recipient_attestations = Some(vec![RecipientAttestation {
        cm: cm_out,
        epk: out_epk,
        ct_hash: out_ct_hash,
        mac: out_mac,
    }]);

    // Build viewer attestation if authority FVK is set
    let (view_attestations, viewer_data) = if let Some(fvk) = input.authority_fvk {
        let (att, _enc) = make_viewer_bundle(
            &fvk,
            &DOMAIN,
            input.value,
            &input.out_rho,
            &out_recipient,
            &sender_id,
            &cm_out,
        );
        (Some(vec![att.clone()]), Some((fvk, att)))
    } else {
        (None, None)
    };

    // Public output
    let public = SpendPublic {
        anchor_root: input.anchor,
        nullifiers: vec![nf],
        withdraw_amount: 0,
        output_commitments: vec![cm_out],
        view_attestations,
        recipient_attestations,
    };

    // === CIRCUIT ABI: private indices ===
    let n_in: usize = 1;
    let n_out: usize = 1;
    let per_in = depth_usize + 4;

    let mut private_indices = Vec::new();
    private_indices.push(2); // spend_sk

    // Input private indices
    let in_base = 6;
    private_indices.push(in_base);     // value_in
    private_indices.push(in_base + 1); // rho_in
    private_indices.push(in_base + 2); // pos_in
    for k in 0..depth_usize {
        private_indices.push(in_base + 3 + k); // siblings
    }

    // Output private indices
    let out_base = 5 + n_in * per_in + 3;
    private_indices.push(out_base);     // value_out
    private_indices.push(out_base + 1); // rho_out
    private_indices.push(out_base + 2); // pk_spend_out
    private_indices.push(out_base + 3); // pk_ivk_out

    // Viewer section: fvk is private
    if viewer_data.is_some() {
        let viewer_section_start = out_base + n_out * 8;
        private_indices.push(viewer_section_start + 2); // fvk
    }

    let mut host = <sov_ligero_adapter::Ligero as Zkvm>::Host::from_args(program_path)
        .with_private_indices(private_indices);

    // === GUEST ABI ARGUMENTS ===
    host.add_hex_arg(hex::encode(DOMAIN));                      // [1] domain
    host.add_hex_arg(hex::encode(input.spend_sk));              // [2] spend_sk
    host.add_str_arg((TREE_DEPTH as u8).to_string());           // [3] depth
    host.add_hex_arg(hex::encode(input.anchor));                // [4] anchor
    host.add_str_arg("1".to_string());                          // [5] n_in

    // Input[0]
    host.add_str_arg(input.value.to_string());                  // value_in
    host.add_hex_arg(hex::encode(input.in_rho));                // rho_in
    host.add_str_arg((input.position as u64).to_string());      // pos
    for s in &input.siblings {
        host.add_hex_arg(hex::encode(s));                       // siblings
    }
    host.add_hex_arg(hex::encode(nf));                          // nullifier

    host.add_str_arg("0".to_string());                          // withdraw_amount
    host.add_str_arg("1".to_string());                          // n_out

    // Output[0]: 8 args
    host.add_str_arg(input.value.to_string());                  // value_out
    host.add_hex_arg(hex::encode(input.out_rho));               // rho_out
    host.add_hex_arg(hex::encode(out_pk_spend));                // pk_spend_out
    host.add_hex_arg(hex::encode(out_pk_ivk));                  // pk_ivk_out
    host.add_hex_arg(hex::encode(cm_out));                      // cm_out
    host.add_hex_arg(hex::encode(out_epk));                     // epk_out
    host.add_hex_arg(hex::encode(out_ct_hash));                 // ct_hash_out
    host.add_hex_arg(hex::encode(out_mac));                     // mac_out

    // Viewer section (Level-B)
    if let Some((fvk, att)) = viewer_data {
        host.add_str_arg("1".to_string());                      // n_viewers
        host.add_hex_arg(hex::encode(att.fvk_commitment));
        host.add_hex_arg(hex::encode(fvk));                     // fvk
        host.add_hex_arg(hex::encode(att.ct_hash));
        host.add_hex_arg(hex::encode(att.mac));
    }

    host.set_public_output(&public).context("set public output")?;
    let proof_bytes = host.run(true).context("generate transfer proof")?;

    Ok(TransferProofOutput {
        proof_bytes,
        nullifier: nf,
        cm_out,
        out_recipient,
        out_pk_spend,
        out_pk_ivk,
        out_epk,
        out_ct_hash,
        out_mac,
    })
}

/// Build encrypted notes for the authority viewer (Level-B), if enabled.
pub fn build_authority_view_ciphertexts(
    authority_fvk: Option<Hash32>,
    value: u128,
    out_rho: &Hash32,
    out_recipient: &Hash32,
    sender_id: &Hash32,
) -> Option<Vec<EncryptedNote>> {
    authority_fvk.map(|fvk| {
        let cm_out = note_commitment(&DOMAIN, value, out_rho, out_recipient);
        let (_att, enc) = make_viewer_bundle(
            &fvk, &DOMAIN, value, out_rho, out_recipient, sender_id, &cm_out,
        );
        vec![enc]
    })
}

/// Build recipient ciphertext for a note (sender-bound).
pub fn build_recipient_ciphertext(
    pk_ivk: &Hash32,
    value: u128,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> Result<midnight_privacy::RecipientCiphertext> {
    let note = Note {
        domain: DOMAIN,
        value,
        rho: *rho,
        recipient: *recipient,
    };
    let cm = note_commitment(&DOMAIN, value, rho, recipient);
    encrypt_note_for_recipient_with_sender(&DOMAIN, pk_ivk, &note, sender_id, &cm)
        .context("Failed to encrypt note for recipient")
}
