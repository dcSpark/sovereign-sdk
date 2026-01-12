use anyhow::{Context, Result};
use borsh;
use demo_stf::runtime::{Runtime, RuntimeCall};
use hex;
use midnight_privacy::{
    nf_key_from_sk, note_commitment, nullifier, pk_from_sk, pk_ivk_from_sk, recipient_from_pk_v2,
    CallMessage, Hash32, MerkleTree, PrivacyAddress, SpendPublic,
};
use rand::Rng;
use serde_json;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_ligero_adapter::Ligero;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
use sov_rollup_ligero::MockDemoRollup;
use sov_test_utils::default_test_signed_transaction;
use std::fs;
use serde::Deserialize;

mod note_spend_guest_v2;
mod rollup_schema;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

#[derive(Deserialize)]
struct NotesResponse {
    notes: Vec<NoteEntry>,
    #[serde(default)]
    current_root: Option<Vec<u8>>,
}

#[derive(Deserialize)]
struct NoteEntry {
    position: u64,
    commitment: Vec<u8>,
}

fn decode_hash32_env(var: &str) -> Result<Hash32> {
    let raw = std::env::var(var).with_context(|| format!("Missing env var {var}"))?;
    let raw = raw.trim();
    let raw = raw.strip_prefix("0x").unwrap_or(raw);
    hex::decode(raw)
        .with_context(|| format!("Invalid hex in env var {var}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid {var} length (expected 32 bytes)"))
}

fn load_notes_from_source() -> Option<NotesResponse> {
    if let Ok(path) = std::env::var("NOTES_FILE") {
        let data = fs::read_to_string(path).ok()?;
        return serde_json::from_str(&data).ok();
    }

    if let Ok(node_url) = std::env::var("NODE_API_URL") {
        // IMPORTANT: fetch ALL notes (pagination) so the Merkle root/path matches on-chain state.
        // Using a small bounded `limit` (like 200) can produce an incomplete tree and invalid proofs.
        let mut all_notes: Vec<NoteEntry> = Vec::new();
        let mut offset: usize = 0;
        let limit: usize = 1000;
        let mut last_root: Option<Vec<u8>> = None;

        loop {
            let url = format!(
                "{}/modules/midnight-privacy/notes?limit={}&offset={}",
                node_url, limit, offset
            );
            let resp = reqwest::blocking::get(&url).ok()?;
            let page = resp.json::<NotesResponse>().ok()?;

            if let Some(root) = page.current_root.clone() {
                last_root = Some(root);
            }

            let n = page.notes.len();
            all_notes.extend(page.notes);
            if n < limit {
                break;
            }
            offset += n;
        }

        return Some(NotesResponse {
            notes: all_notes,
            current_root: last_root,
        });
    }
    None
}

fn main() -> Result<()> {
    let domain: Hash32 = decode_hash32_env("NOTE_DOMAIN")?;
    let value: u128 = std::env::var("NOTE_VALUE")
        .with_context(|| "Missing env var NOTE_VALUE")?
        .parse()
        .context("Invalid NOTE_VALUE")?;
    let rho: Hash32 = decode_hash32_env("NOTE_RHO")?;
    let spend_sk: Hash32 = decode_hash32_env("NOTE_SPEND_SK")?;
    let out1_value: u128 = std::env::var("TRANSFER_OUT1")?.parse()?;
    let out2_value: u128 = std::env::var("TRANSFER_OUT2")?.parse()?;
    let position: u64 = std::env::var("NOTE_POSITION")?.parse()?;
    let nonce: u64 = std::env::var("NONCE")?.parse()?;
    let node_url =
        std::env::var("NODE_API_URL").unwrap_or_else(|_| "http://localhost:12346".to_string());
    let chain_hash = rollup_schema::fetch_rollup_chain_hash(&node_url)?;
    println!(
        "Chain hash (/rollup/schema): 0x{}",
        hex::encode(chain_hash)
    );
    anyhow::ensure!(
        out1_value + out2_value == value,
        "Transfer outputs must sum to input value: in={} out1={} out2={}",
        value,
        out1_value,
        out2_value
    );

    let anchor_bytes: Vec<u8> = serde_json::from_str(&std::env::var("ANCHOR_ROOT")?)?;
    let mut anchor: Hash32 = anchor_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid anchor"))?;

    let pk_spend_owner = pk_from_sk(&spend_sk);
    let pk_ivk_owner = pk_ivk_from_sk(&domain, &spend_sk);
    let in_recipient = recipient_from_pk_v2(&domain, &pk_spend_owner, &pk_ivk_owner);
    let in_sender_id = in_recipient; // deposit-created notes use sender_id = recipient
    let value_u64 = u64::try_from(value).context("NOTE_VALUE too large")?;
    // Deposit-created notes use sender_id = recipient.
    let cm = note_commitment(&domain, value_u64, &rho, &in_recipient, &in_sender_id);
    let nf_key = nf_key_from_sk(&domain, &spend_sk);
    let nf = nullifier(&domain, &nf_key, &rho);

    // Build tree with on-chain notes when available; otherwise fall back to single-leaf tree.
    let tree_depth: u8 = 16;
    let mut tree = MerkleTree::new(tree_depth);
    if let Some(notes_resp) = load_notes_from_source() {
        // Prefer the fresh root reported by the node (avoid stale ANCHOR_ROOT from earlier script steps).
        if let Some(root) = notes_resp.current_root.clone() {
            if let Ok(root32) = <[u8; 32]>::try_from(root) {
                let fresh: Hash32 = root32;
                if fresh != anchor {
                    println!("ℹ️  Provided ANCHOR_ROOT differs from node current_root; using node root");
                }
                anchor = fresh;
            }
        }
        for note in notes_resp.notes {
            if note.position >= (1u64 << tree_depth) {
                continue;
            }
            if let Ok(commitment) = note.commitment.clone().try_into() {
                tree.set_leaf(note.position as usize, commitment);
            }
        }
        // Ensure our note is present (in case not returned due to limit/filter).
        tree.set_leaf(position as usize, cm);
        let computed_root: Hash32 = tree
            .root()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Computed root has invalid length"))?;
        anyhow::ensure!(
            computed_root == anchor,
            "Anchor root mismatch: env/node anchor = 0x{}, but Merkle root computed from on-chain notes = 0x{}. \
             This usually means your note isn't fully indexed yet, or the note list is incomplete.",
            hex::encode(anchor),
            hex::encode(computed_root),
        );
    } else {
        tree.set_leaf(position as usize, cm);
    }
    let siblings = tree.open(position as usize);

    println!("Transfer: {} → {} + {}", value, out1_value, out2_value);
    println!("Input nullifier: 0x{}", hex::encode(&nf[..8]));

    // Create 2 output notes (pure shielded transfer, withdraw_amount = 0).
    // Output sender_id is the spender's privacy identity.
    let sender_id_out = in_recipient;

    let out1_rho: Hash32 = rand::thread_rng().gen();
    let out1_spend_sk: Hash32 = rand::thread_rng().gen();
    let out1_pk_spend: Hash32 = pk_from_sk(&out1_spend_sk);
    let out1_pk_ivk: Hash32 = pk_ivk_from_sk(&domain, &out1_spend_sk);
    let out1_recipient: Hash32 = recipient_from_pk_v2(&domain, &out1_pk_spend, &out1_pk_ivk);
    let cm_out1 = note_commitment(
        &domain,
        u64::try_from(out1_value).context("TRANSFER_OUT1 too large")?,
        &out1_rho,
        &out1_recipient,
        &sender_id_out,
    );

    let out2_rho: Hash32 = rand::thread_rng().gen();
    let out2_spend_sk: Hash32 = rand::thread_rng().gen();
    let out2_pk_spend: Hash32 = pk_from_sk(&out2_spend_sk);
    let out2_pk_ivk: Hash32 = pk_ivk_from_sk(&domain, &out2_spend_sk);
    let out2_recipient: Hash32 = recipient_from_pk_v2(&domain, &out2_pk_spend, &out2_pk_ivk);
    let cm_out2 = note_commitment(
        &domain,
        u64::try_from(out2_value).context("TRANSFER_OUT2 too large")?,
        &out2_rho,
        &out2_recipient,
        &sender_id_out,
    );

    // Save first output details for withdrawal step
    let out1_addr = PrivacyAddress::from_keys(&out1_pk_spend, &out1_pk_ivk);
    let out1_details = serde_json::json!({
        "domain": hex::encode(domain),
        "amount": out1_value,
        "rho": hex::encode(out1_rho),
        "sender_id": hex::encode(sender_id_out),
        "privacy_address": out1_addr.to_string(),
        "pk_spend": hex::encode(out1_pk_spend),
        "pk_ivk": hex::encode(out1_pk_ivk),
        "recipient": hex::encode(out1_recipient),
        "commitment": hex::encode(cm_out1),
        "spend_sk": hex::encode(out1_spend_sk),
        "nf_key": hex::encode(nf_key_from_sk(&domain, &out1_spend_sk)) // derived (debug)
    });
    fs::write(
        "midnight_transfer_out1_details.json",
        serde_json::to_string_pretty(&out1_details)?,
    )?;

    let public_output = SpendPublic {
        anchor_root: anchor,
        // Filled after fetching deny-map openings (defaults to all-allowed when NODE_API_URL is unset).
        blacklist_root: midnight_privacy::default_blacklist_root(),
        nullifier: nf,
        withdraw_amount: 0, // Pure shielded transfer
        output_commitments: vec![cm_out1, cm_out2],
        view_attestations: None,
    };

    let program_path = std::env::var("LIGERO_PROGRAM_PATH")?;
    let packing: u32 = std::env::var("LIGERO_PACKING")
        .unwrap_or_else(|_| "8192".to_string())
        .parse()
        .context("Invalid LIGERO_PACKING")?;

    // note_spend_guest v2 ABI builder (includes inv_enforce + deny-map section).
    let input = note_spend_guest_v2::SpendInputV2 {
        value: value_u64,
        rho,
        sender_id: in_sender_id,
        pos: position,
        siblings: siblings.clone(),
        nullifier: nf,
    };

    let out1 = note_spend_guest_v2::SpendOutputV2 {
        value: u64::try_from(out1_value).context("TRANSFER_OUT1 too large")?,
        rho: out1_rho,
        pk_spend: out1_pk_spend,
        pk_ivk: out1_pk_ivk,
        cm: cm_out1,
    };
    let out2 = note_spend_guest_v2::SpendOutputV2 {
        value: u64::try_from(out2_value).context("TRANSFER_OUT2 too large")?,
        rho: out2_rho,
        pk_spend: out2_pk_spend,
        pk_ivk: out2_pk_ivk,
        cm: cm_out2,
    };

    // Deny-map root + openings:
    // - always: sender_id
    // - transfer only (withdraw_amount == 0): pay recipient (output 0)
    // Change outputs (output 1 when n_out==2) are enforced to be self in-circuit and are not checked separately.
    let sender_addr = PrivacyAddress::from_keys(&pk_spend_owner, &pk_ivk_owner);
    let addr_list = vec![sender_addr, PrivacyAddress::from_keys(&out1_pk_spend, &out1_pk_ivk)];
    let node_url = std::env::var("NODE_API_URL").ok();
    let (blacklist_root, deny_openings) =
        note_spend_guest_v2::deny_map_openings_or_default(node_url.as_deref(), &addr_list)?;

    let (args, private_indices) = note_spend_guest_v2::build_note_spend_args_v2(
        domain,
        spend_sk,
        pk_ivk_owner,
        tree_depth,
        anchor,
        &[input],
        0,            // withdraw_amount
        [0u8; 32],    // withdraw_to (unused for transfers)
        &[out1, out2],
        blacklist_root,
        &deny_openings,
    )?;

    let mut host = <Ligero as Zkvm>::Host::from_args(&program_path)
        .with_packing(packing)
        .with_private_indices(private_indices);
    note_spend_guest_v2::add_args_to_host(&mut host, &args)?;

    let mut public_output = public_output;
    public_output.blacklist_root = blacklist_root;
    host.set_public_output(&public_output)?;

    println!("Generating proof...");
    let proof_bytes = host
        .run(true)
        .context("Ligero prover did not produce a valid proof")?;
    println!("✓ Proof generated: {} bytes", proof_bytes.len());

    let key_data: PrivateKeyAndAddress<DemoRollupSpec> =
        serde_json::from_str(&fs::read_to_string(std::env::var("PRIVATE_KEY_FILE")?)?)?;

    let proof_safe = proof_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large"))?;

    let msg = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(CallMessage::Transfer {
        proof: proof_safe,
        anchor_root: anchor,
        nullifier: nf,
        gas: None,
        view_ciphertexts: None,
    });

    let tx: Transaction<Runtime<DemoRollupSpec>, DemoRollupSpec> =
        default_test_signed_transaction(&key_data.private_key, &msg, nonce, &chain_hash);

    let tx_bytes = borsh::to_vec(&tx)?;
    fs::write("midnight_transfer_tx.bin", &tx_bytes)?;

    let tx_base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes);
    let tx_json = serde_json::json!({"body": tx_base64});

    fs::write(
        "midnight_transfer_tx.json",
        serde_json::to_string_pretty(&tx_json)?,
    )?;

    println!("✓ Transaction ready: midnight_transfer_tx.json");
    Ok(())
}
