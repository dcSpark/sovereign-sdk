use anyhow::{anyhow, Context, Result};
use borsh;
use demo_stf::runtime::{Runtime, RuntimeCall};
use hex;
use midnight_privacy::{
    nf_key_from_sk, note_commitment, nullifier, pk_from_sk, pk_ivk_from_sk, recipient_from_pk_v2,
    CallMessage, EncryptedNote, Hash32, MerkleTree, PrivacyAddress, SpendPublic, ViewAttestation,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::Transaction;
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_ligero::MockDemoRollup;
use sov_test_utils::default_test_signed_transaction;
use std::fs;

mod note_spend_guest_v2;
mod rollup_schema;

type DemoRollupSpec = <MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

/// Inject pool signature into the Nightstream proof package.
///
/// Decompresses the DEFLATE-compressed proof, deserializes the package,
/// extracts the FVK commitment from SpendPublic.view_attestations, sets
/// the pool_viewer_sig field, and re-serializes + recompresses.
fn inject_pool_sig_hex_into_proof_bytes(
    proof_bytes: Vec<u8>,
    _fvk_commitment_arg_pos: usize,
    pool_sig_hex: String,
) -> Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use sov_nightstream_adapter::{NightstreamProofPackage, PoolViewerSig};
    use std::io::{Read, Write};

    let sig_bytes = hex::decode(pool_sig_hex.trim())
        .context("pool_sig_hex is not valid hex")?;
    anyhow::ensure!(sig_bytes.len() == 64, "pool_sig_hex must be 64 bytes (got {})", sig_bytes.len());

    let decompressed = {
        let mut decoder = DeflateDecoder::new(proof_bytes.as_slice());
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).context("Failed to decompress proof bytes")?;
        buf
    };

    let mut package: NightstreamProofPackage =
        bincode::deserialize(&decompressed).context("Failed to deserialize NightstreamProofPackage")?;

    let public: midnight_privacy::SpendPublic =
        bincode::deserialize(&package.public_output)
            .context("Failed to deserialize SpendPublic from package.public_output")?;

    let fvk_commitment = public
        .view_attestations
        .as_ref()
        .and_then(|atts| atts.first())
        .map(|att| att.fvk_commitment)
        .ok_or_else(|| anyhow::anyhow!("Cannot inject pool sig: no view_attestations in SpendPublic"))?;

    package.pool_viewer_sig = Some(PoolViewerSig {
        fvk_commitment,
        signature: sig_bytes,
    });

    let raw = bincode::serialize(&package).context("Failed to re-serialize NightstreamProofPackage")?;

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).context("Failed to write to deflate encoder")?;
    encoder.finish().context("Failed to finish deflate compression")
}

/// Length of note plaintext for transfers: 32(domain) + 16(value) + 32(rho) + 32(recipient) + 32(sender_id)
const NOTE_PLAIN_LEN_TRANSFER: usize = 144;

/// Request body for the prover service
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProverServiceRequest {
    circuit: String,
    args: serde_json::Value,
    private_indices: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packing: Option<u32>,
}

/// Response from the prover service
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProverServiceResponse {
    success: bool,
    exit_code: i32,
    proof: Option<String>,
}

/// Generate proof using the remote Nightstream prover service.
fn prove_with_service(
    service_url: &str,
    _program_path: &str,
    _args: &[serde_json::Value],
    _private_indices: Vec<usize>,
    _packing: u32,
    public_output: &[u8],
) -> Result<Vec<u8>> {
    let public: SpendPublic =
        bincode::deserialize(public_output).context("Invalid SpendPublic for prover request")?;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300)) // 5 min timeout for proving
        .build()
        .context("Failed to create HTTP client")?;

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ProveRequest<'a> {
        spend_public: &'a SpendPublic,
        binary: bool,
    }

    let request = ProveRequest {
        spend_public: &public,
        binary: true,
    };

    let url = format!("{}/prove", service_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .json(&request)
        .send()
        .context("Failed to send request to prover service")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        anyhow::bail!("Prover service failed (status={}): {}", status, body);
    }

    let proof_bytes = response
        .bytes()
        .context("Failed to read proof bytes from response")?
        .to_vec();

    anyhow::ensure!(
        !proof_bytes.is_empty(),
        "Prover service returned empty proof"
    );

    Ok(proof_bytes)
}

/// Helper to create an EncryptedNote for the transaction (matching mcp-external/viewer.rs)
fn create_encrypted_note(
    fvk: &Hash32,
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
    cm: &Hash32,
) -> EncryptedNote {
    use midnight_privacy::viewing::{ct_hash, fvk_commitment, view_kdf, view_mac};
    use midnight_privacy::FullViewingKey;

    let fvk_obj = FullViewingKey(*fvk);
    let fvk_c = fvk_commitment(&fvk_obj);

    // Encode plaintext
    let mut pt = [0u8; NOTE_PLAIN_LEN_TRANSFER];
    pt[0..32].copy_from_slice(domain);
    pt[32..40].copy_from_slice(&value.to_le_bytes());
    pt[40..48].copy_from_slice(&[0u8; 8]);
    pt[48..80].copy_from_slice(rho);
    pt[80..112].copy_from_slice(recipient);
    pt[112..144].copy_from_slice(sender_id);

    // Encrypt with Poseidon-based keystream
    let k = view_kdf(&fvk_obj, cm);
    let mut ct = [0u8; NOTE_PLAIN_LEN_TRANSFER];

    // Stream XOR encryption
    let block_fn = |ctr: u32| -> Hash32 {
        let c = ctr.to_le_bytes();
        midnight_privacy::poseidon2_hash(b"VIEW_STREAM_V1", &[&k, &c])
    };
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < pt.len() {
        let ks = block_fn(ctr);
        ctr = ctr.wrapping_add(1);
        let take = core::cmp::min(32, pt.len() - off);
        for i in 0..take {
            ct[off + i] = pt[off + i] ^ ks[i];
        }
        off += take;
    }

    let ct_h = ct_hash(&ct);
    let mac = view_mac(&k, cm, &ct_h);

    EncryptedNote {
        cm: *cm,
        nonce: [0u8; 24],
        ct: sov_modules_api::SafeVec::try_from(ct.to_vec()).expect("ciphertext within limit"),
        fvk_commitment: fvk_c,
        mac,
    }
}

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

    // Change output (out2) goes back to the spender, so use spender's own keys.
    // This matches transfer.rs in mcp-external: change uses pk_spend_owner & pk_ivk_owner.
    let out2_rho: Hash32 = rand::thread_rng().gen();
    let out2_pk_spend: Hash32 = pk_spend_owner;
    let out2_pk_ivk: Hash32 = pk_ivk_owner;
    let out2_recipient: Hash32 = in_recipient; // spender's recipient (same as sender_id_out)
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
        // Filled after fetching deny-map openings.
        blacklist_root: midnight_privacy::default_blacklist_root(),
        nullifiers: vec![nf],
        withdraw_amount: 0, // Pure shielded transfer
        output_commitments: vec![cm_out1, cm_out2],
        view_attestations: None,
    };

    let program_path = std::env::var("NIGHTSTREAM_PROGRAM_PATH")
        .or_else(|_| std::env::var("LIGERO_PROGRAM_PATH"))
        .unwrap_or_else(|_| "note_spend_guest".to_string());

    // Deny-map root + openings:
    // - always: sender_id
    // - transfer only (withdraw_amount == 0): pay recipient (output 0)
    // Change outputs (output 1 when n_out==2) are enforced to be self in-circuit and are not checked separately.
    let sender_addr = PrivacyAddress::from_keys(&pk_spend_owner, &pk_ivk_owner);
    let addr_list = vec![sender_addr, PrivacyAddress::from_keys(&out1_pk_spend, &out1_pk_ivk)];
    let (blacklist_root, deny_openings) =
        note_spend_guest_v2::fetch_deny_map_openings(&node_url, &addr_list)?;

    // Check for FVK bundle (new: POOL_FVK_PK + FVK service) or authority FVK (deprecated)
    let fvk_bundle = note_spend_guest_v2::load_fvk_bundle();
    let authority_fvk = fvk_bundle.as_ref().map(|b| b.fvk).or_else(note_spend_guest_v2::load_authority_fvk);
    let (viewer_atts, view_attestations_pub, view_ciphertexts, viewer_fvk_for_circuit) = if let Some(fvk) = authority_fvk {
        if fvk_bundle.is_some() {
            println!("POOL_FVK_PK configured: generating viewer attestations for 2 output(s) (with pool signature)");
        } else {
            println!("AUTHORITY_FVK configured (deprecated): generating viewer attestations for 2 output(s)");
        }

        let out1_value_u64 = u64::try_from(out1_value).context("TRANSFER_OUT1 too large")?;
        let out2_value_u64 = u64::try_from(out2_value).context("TRANSFER_OUT2 too large")?;

        let att1 = note_spend_guest_v2::make_viewer_attestation(
            &fvk,
            &domain,
            out1_value_u64,
            &out1_rho,
            &out1_recipient,
            &sender_id_out,
            &cm_out1,
        );
        let att2 = note_spend_guest_v2::make_viewer_attestation(
            &fvk,
            &domain,
            out2_value_u64,
            &out2_rho,
            &out2_recipient,
            &sender_id_out,
            &cm_out2,
        );

        // Create full ViewAttestations for SpendPublic
        let va1 = ViewAttestation {
            cm: cm_out1,
            fvk_commitment: att1.fvk_commitment,
            ct_hash: att1.ct_hash,
            mac: att1.mac,
        };
        let va2 = ViewAttestation {
            cm: cm_out2,
            fvk_commitment: att2.fvk_commitment,
            ct_hash: att2.ct_hash,
            mac: att2.mac,
        };

        // Create EncryptedNote entries for the transaction
        let enc1 = create_encrypted_note(&fvk, &domain, out1_value_u64, &out1_rho, &out1_recipient, &sender_id_out, &cm_out1);
        let enc2 = create_encrypted_note(&fvk, &domain, out2_value_u64, &out2_rho, &out2_recipient, &sender_id_out, &cm_out2);

        (
            Some(vec![att1, att2]),
            Some(vec![va1, va2]),
            Some(vec![enc1, enc2]),
            Some(fvk),
        )
    } else {
        println!("No FVK configured (set POOL_FVK_PK or AUTHORITY_FVK): transfer will not include viewer attestation");
        (None, None, None, None)
    };

    let mut public_output = public_output;
    public_output.blacklist_root = blacklist_root;
    public_output.view_attestations = view_attestations_pub;

    // Serialize public output for the proof package
    let public_output_bytes =
        bincode::serialize(&public_output).context("Failed to serialize public output")?;

    // Check if we should use the remote prover service
    let prover_service_url = std::env::var("PROVER_SERVICE_URL").ok();

    // Compute inv_enforce for the enforce-product check.
    let out1_value_u64 = u64::try_from(out1_value).context("TRANSFER_OUT1 too large for u64")?;
    let out2_value_u64 = u64::try_from(out2_value).context("TRANSFER_OUT2 too large for u64")?;
    let inv_enforce = midnight_privacy::inv_enforce_v2(
        &[value_u64],
        &[rho],
        &[out1_value_u64, out2_value_u64],
        &[out1_rho, out2_rho],
    );

    // Build the full circuit witness.
    use sov_nightstream_adapter::{
        BlacklistProof, NoteSpendInput, NoteSpendOutput, NoteSpendWitness,
        ViewerOutputWitness, ViewerWitness,
    };

    let witness = NoteSpendWitness {
        domain,
        spend_sk,
        pk_ivk_owner,
        depth: tree_depth as u32,
        anchor,
        inputs: vec![NoteSpendInput {
            value: value_u64,
            rho,
            sender_id: in_sender_id,
            position: position as u32,
            siblings: siblings.clone(),
            nullifier: nf,
        }],
        withdraw_amount: 0,
        withdraw_to: [0u8; 32],
        outputs: vec![
            NoteSpendOutput {
                value: out1_value_u64,
                rho: out1_rho,
                pk_spend: out1_pk_spend,
                pk_ivk: out1_pk_ivk,
                cm: cm_out1,
            },
            NoteSpendOutput {
                value: out2_value_u64,
                rho: out2_rho,
                pk_spend: out2_pk_spend,
                pk_ivk: out2_pk_ivk,
                cm: cm_out2,
            },
        ],
        inv_enforce,
        blacklist_root,
        blacklist_proofs: vec![
            BlacklistProof::from_opening(
                &in_recipient,
                deny_openings[0].bucket_entries,
                deny_openings[0].siblings.clone(),
            ),
            BlacklistProof::from_opening(
                &out1_recipient,
                deny_openings[1].bucket_entries,
                deny_openings[1].siblings.clone(),
            ),
        ],
        viewers: if let (Some(ref atts), Some(fvk)) = (&viewer_atts, viewer_fvk_for_circuit) {
            vec![ViewerWitness {
                fvk_commitment: atts[0].fvk_commitment,
                fvk,
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
        },
    };

    let mut proof_bytes = if let Some(service_url) = prover_service_url {
        println!("Generating proof via Nightstream prover service ({})...", service_url);
        prove_with_service(
            &service_url,
            &program_path,
            &[],
            vec![],
            0,
            &public_output_bytes,
        )?
    } else {
        use sov_nightstream_adapter::circuits::note_spend_rom;
        use sov_nightstream_adapter::NightstreamHost;
        use sov_rollup_interface::zk::ZkvmHost;

        let mut host = NightstreamHost::new(
            &note_spend_rom::NOTE_SPEND_ROM,
            note_spend_rom::NOTE_SPEND_ROM_BASE,
        );
        host.write_note_spend_witness(&witness, public_output_bytes.clone());

        println!("Generating proof (local Nightstream)...");
        host.run(true)
            .context("Nightstream prover did not produce a valid proof")?
    };
    println!("✓ Proof generated: {} bytes", proof_bytes.len());

    // Inject pool signature if FVK bundle is available (POOL_FVK_PK mode)
    if let Some(ref bundle) = fvk_bundle {
        if viewer_atts.is_some() {
            // Calculate fvk_commitment position in args
            // Structure: header(6) + inputs(4+depth+1 per input) + withdraw(3) + outputs(5*n_out) + inv_enforce(1) + blacklist(1+openings) + viewer(n_viewers=1, fvk_commitment, ...)
            let tree_depth: u8 = 16;
            let depth_usize = tree_depth as usize;
            let n_in = 1usize;
            let n_out = 2usize;
            let inputs_args = n_in * (4 + depth_usize + 1); // value, rho, sender_id, pos, siblings, nullifier
            let withdraw_args = 3; // withdraw_amount, withdraw_to, n_out
            let outputs_args = n_out * 5; // value, rho, pk_spend, pk_ivk, cm per output
            let inv_enforce_args = 1;
            let bl_depth = midnight_privacy::BLACKLIST_TREE_DEPTH as usize;
            let bl_per_check = midnight_privacy::BLACKLIST_BUCKET_SIZE + 1 + bl_depth;
            let bl_checks = 2usize; // sender + pay recipient for transfers
            let blacklist_args = 1 + bl_checks * bl_per_check; // root + openings

            // fvk_commitment is at: header(6) + inputs + withdraw + outputs + inv_enforce + blacklist + n_viewers(1) + fvk_commitment
            let fvk_commitment_arg_pos = 6 + inputs_args + withdraw_args + outputs_args + inv_enforce_args + blacklist_args + 1 + 1;

            proof_bytes = inject_pool_sig_hex_into_proof_bytes(
                proof_bytes,
                fvk_commitment_arg_pos,
                bundle.pool_sig_hex.clone(),
            )?;
            println!("✓ Pool signature injected into proof");
        }
    }

    let key_data: PrivateKeyAndAddress<DemoRollupSpec> =
        serde_json::from_str(&fs::read_to_string(std::env::var("PRIVATE_KEY_FILE")?)?)?;

    let proof_safe = proof_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Proof too large"))?;

    let msg = RuntimeCall::<DemoRollupSpec>::MidnightPrivacy(CallMessage::Transfer {
        proof: proof_safe,
        anchor_root: anchor,
        nullifiers: vec![nf],
        gas: None,
        view_ciphertexts,
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
