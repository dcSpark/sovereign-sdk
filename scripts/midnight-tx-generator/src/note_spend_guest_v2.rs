use anyhow::{Context, Result};
use ligetron::bn254fr_native::submod_checked;
use ligetron::Bn254Fr;
use midnight_privacy::{
    default_blacklist_root, sparse_default_nodes,
    viewing::{ct_hash, fvk_commitment, view_kdf, view_mac},
    BlacklistBucketEntries, BlacklistOpeningResponse, FullViewingKey, Hash32, PrivacyAddress,
    BLACKLIST_TREE_DEPTH,
};
use serde_json::json;
use sov_ligero_adapter::LigeroHost;

fn hex32(h: &Hash32) -> String {
    hex::encode(h)
}

#[derive(Debug, Clone)]
pub struct SpendInputV2 {
    pub value: u64,
    pub rho: Hash32,
    pub sender_id: Hash32,
    pub pos: u64,
    pub siblings: Vec<Hash32>,
    pub nullifier: Hash32,
}

#[derive(Debug, Clone)]
pub struct SpendOutputV2 {
    pub value: u64,
    pub rho: Hash32,
    pub pk_spend: Hash32,
    pub pk_ivk: Hash32,
    pub cm: Hash32,
}

fn u64_to_i64(v: u64, label: &'static str) -> Result<i64> {
    i64::try_from(v).with_context(|| {
        format!("{label} does not fit into i64 (required by note_spend_guest v2 ABI)")
    })
}

#[derive(Debug, Clone)]
pub struct DenyMapOpeningV2 {
    pub bucket_entries: BlacklistBucketEntries,
    pub siblings: Vec<Hash32>,
}

fn bn254fr_from_hash32_be(h: &Hash32) -> Bn254Fr {
    let mut out = Bn254Fr::new();
    out.set_bytes_big(h);
    out
}

fn bl_bucket_inv_for_id(id: &Hash32, bucket_entries: &BlacklistBucketEntries) -> Result<Hash32> {
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

/// Viewer attestation data for Level-B viewing support.
#[derive(Debug, Clone)]
pub struct ViewerAttestationV2 {
    pub fvk_commitment: Hash32,
    pub ct_hash: Hash32,
    pub mac: Hash32,
}

/// Length of note plaintext for transfers: 32(domain) + 16(value) + 32(rho) + 32(recipient) + 32(sender_id)
const NOTE_PLAIN_LEN_TRANSFER: usize = 144;

/// Produce the i-th 32-byte stream block for key k using Poseidon2.
fn stream_block(k: &Hash32) -> impl Fn(u32) -> Hash32 + '_ {
    move |ctr: u32| {
        let c = ctr.to_le_bytes();
        midnight_privacy::poseidon2_hash(b"VIEW_STREAM_V1", &[k, &c])
    }
}

/// SNARK-friendly deterministic encryption: XOR plaintext with Poseidon-based keystream.
fn stream_xor_encrypt(k: &Hash32, pt: &[u8], ct_out: &mut [u8]) {
    debug_assert_eq!(pt.len(), ct_out.len());
    let block_fn = stream_block(k);
    let mut ctr = 0u32;
    let mut off = 0usize;
    while off < pt.len() {
        let ks = block_fn(ctr);
        ctr = ctr.wrapping_add(1);
        let take = core::cmp::min(32, pt.len() - off);
        for i in 0..take {
            ct_out[off + i] = pt[off + i] ^ ks[i];
        }
        off += take;
    }
}

/// Serialize note plaintext for encryption (144 bytes with sender_id).
fn encode_note_plain(
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
) -> [u8; NOTE_PLAIN_LEN_TRANSFER] {
    let mut out = [0u8; NOTE_PLAIN_LEN_TRANSFER];
    out[0..32].copy_from_slice(domain);
    // Encode as 16-byte LE, zero-extended from u64.
    out[32..40].copy_from_slice(&value.to_le_bytes());
    out[40..48].copy_from_slice(&[0u8; 8]);
    out[48..80].copy_from_slice(rho);
    out[80..112].copy_from_slice(recipient);
    out[112..144].copy_from_slice(sender_id);
    out
}

/// Create a viewer attestation for a note (matching `make_viewer_bundle` in mcp-external).
///
/// # Arguments
/// * `vfk` - The Full Viewing Key (32-byte secret)
/// * `domain` - The note domain
/// * `value` - The token amount (as u64)
/// * `rho` - The note randomness
/// * `recipient` - The recipient identifier
/// * `sender_id` - The sender identifier (spender's address for transfers)
/// * `cm` - The note commitment
///
/// # Returns
/// A ViewerAttestationV2 containing the attestation data for the ZK proof
pub fn make_viewer_attestation(
    vfk: &Hash32,
    domain: &Hash32,
    value: u64,
    rho: &Hash32,
    recipient: &Hash32,
    sender_id: &Hash32,
    cm: &Hash32,
) -> ViewerAttestationV2 {
    let vfk_obj = FullViewingKey(*vfk);
    let vfk_c = fvk_commitment(&vfk_obj);
    let pt = encode_note_plain(domain, value, rho, recipient, sender_id);
    let k = view_kdf(&vfk_obj, cm);
    let mut ct = [0u8; NOTE_PLAIN_LEN_TRANSFER];
    stream_xor_encrypt(&k, &pt, &mut ct);
    let ct_h = ct_hash(&ct);
    let mac = view_mac(&k, cm, &ct_h);

    ViewerAttestationV2 {
        fvk_commitment: vfk_c,
        ct_hash: ct_h,
        mac,
    }
}

/// Load authority viewing key from environment variable AUTHORITY_VFK.
///
/// Accepts hex strings with or without `0x` prefix.
/// Returns `None` if:
/// - Environment variable is not set
/// - Hex decoding fails
/// - Length is not exactly 32 bytes
pub fn load_authority_vfk() -> Option<Hash32> {
    let raw = std::env::var("AUTHORITY_VFK").ok()?;
    let s = raw.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = match hex::decode(s) {
        Ok(b) => b,
        Err(_) => return None,
    };
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

pub fn add_args_to_host(host: &mut LigeroHost, args: &[serde_json::Value]) -> Result<()> {
    for a in args {
        if let Some(hex) = a.get("hex").and_then(|v| v.as_str()) {
            host.add_hex_arg(hex.to_string());
            continue;
        }
        if let Some(i64v) = a.get("i64").and_then(|v| v.as_i64()) {
            host.add_i64_arg(i64v);
            continue;
        }
        if let Some(s) = a.get("str").and_then(|v| v.as_str()) {
            host.add_str_arg(s.to_string());
            continue;
        }
        // Support for HexBytesB64 format (both hex and bytes_b64 present)
        if let (Some(hex), Some(_bytes_b64)) = (
            a.get("hex").and_then(|v| v.as_str()),
            a.get("bytes_b64").and_then(|v| v.as_str()),
        ) {
            host.add_hex_arg(hex.to_string());
            continue;
        }
        anyhow::bail!("Unexpected Ligero arg JSON shape: {a}");
    }
    Ok(())
}

#[allow(dead_code)]
pub fn default_deny_map_openings(n: usize) -> (Hash32, Vec<DenyMapOpeningV2>) {
    let bl_depth = BLACKLIST_TREE_DEPTH as usize;
    let bl_defaults = sparse_default_nodes(BLACKLIST_TREE_DEPTH);
    let default_siblings: Vec<Hash32> = bl_defaults.iter().take(bl_depth).copied().collect();
    let blacklist_root = default_blacklist_root();
    let opening = DenyMapOpeningV2 {
        bucket_entries: midnight_privacy::empty_blacklist_bucket_entries(),
        siblings: default_siblings,
    };
    (blacklist_root, vec![opening; n])
}

pub fn fetch_deny_map_openings(
    node_url: &str,
    addrs: &[PrivacyAddress],
) -> Result<(Hash32, Vec<DenyMapOpeningV2>)> {
    let node_url = node_url.trim_end_matches('/');
    let bl_depth = BLACKLIST_TREE_DEPTH as usize;
    let mut root: Option<Hash32> = None;
    let mut out: Vec<DenyMapOpeningV2> = Vec::with_capacity(addrs.len());

    for addr in addrs {
        let url = format!(
            "{}/modules/midnight-privacy/blacklist/opening/{}",
            node_url, addr
        );
        let resp = reqwest::blocking::get(&url)
            .with_context(|| format!("Failed to GET {url}"))?
            .error_for_status()
            .with_context(|| format!("Deny-map opening query failed for {addr}"))?;

        let opening: BlacklistOpeningResponse = resp
            .json()
            .with_context(|| format!("Failed to parse deny-map opening response for {addr}"))?;

        if let Some(r) = root {
            anyhow::ensure!(
                opening.blacklist_root == r,
                "Deny-map root changed while fetching openings (expected {}, got {})",
                hex::encode(r),
                hex::encode(opening.blacklist_root)
            );
        } else {
            root = Some(opening.blacklist_root);
        }

        anyhow::ensure!(
            opening.siblings.len() == bl_depth,
            "deny-map opening for {addr} has wrong sibling length: got {}, expected {}",
            opening.siblings.len(),
            bl_depth
        );
        anyhow::ensure!(
            !opening.is_blacklisted,
            "Privacy address is frozen (blacklisted): {addr}"
        );
        out.push(DenyMapOpeningV2 {
            bucket_entries: opening.bucket_entries,
            siblings: opening.siblings,
        });
    }

    Ok((root.unwrap_or_else(default_blacklist_root), out))
}

#[allow(dead_code)]
pub fn deny_map_openings_or_default(
    node_url: Option<&str>,
    addrs: &[PrivacyAddress],
) -> Result<(Hash32, Vec<DenyMapOpeningV2>)> {
    Ok(match node_url {
        Some(url) => fetch_deny_map_openings(url, addrs)?,
        None => default_deny_map_openings(addrs.len()),
    })
}

/// Encode a transparent rollup address into the 32-byte `withdraw_to` value bound by the circuit.
///
/// The guest binds `withdraw_to` as raw bytes (split into two 16-byte chunks), so we must provide a
/// deterministic 32-byte encoding for variable-length address types.
///
/// Current convention:
/// - Left-pad with zeros to 32 bytes (`withdraw_to[32-len..] = addr_bytes`)
/// - Reject addresses longer than 32 bytes
#[allow(dead_code)] // used by withdrawal generators (not all bins)
pub fn withdraw_to_from_address_bytes(addr_bytes: &[u8]) -> Result<Hash32> {
    anyhow::ensure!(
        addr_bytes.len() <= 32,
        "transparent address too long for withdraw_to: {} bytes (max 32)",
        addr_bytes.len()
    );
    let mut out = [0u8; 32];
    let start = 32 - addr_bytes.len();
    out[start..].copy_from_slice(addr_bytes);
    Ok(out)
}

/// Build note spend arguments with viewer support.
///
/// When `authority_vfk` and `view_attestations` are provided, the viewer section
/// is appended to the arguments (matching `transfer.rs` in mcp-external).
#[allow(dead_code)]
pub fn build_note_spend_args_v2(
    domain: Hash32,
    spend_sk: Hash32,
    pk_ivk_owner: Hash32,
    depth: u8,
    anchor: Hash32,
    inputs: &[SpendInputV2],
    withdraw_amount: u64,
    withdraw_to: Hash32,
    outputs: &[SpendOutputV2],
    blacklist_root: Hash32,
    deny_map_openings: &[DenyMapOpeningV2],
) -> Result<(Vec<serde_json::Value>, Vec<usize>)> {
    build_note_spend_args_v2_with_viewer(
        domain,
        spend_sk,
        pk_ivk_owner,
        depth,
        anchor,
        inputs,
        withdraw_amount,
        withdraw_to,
        outputs,
        blacklist_root,
        deny_map_openings,
        None, // No viewer
        None, // No attestations
    )
}

/// Build note spend arguments with optional viewer section support.
///
/// This is the full implementation that matches `transfer.rs` in mcp-external.
pub fn build_note_spend_args_v2_with_viewer(
    domain: Hash32,
    spend_sk: Hash32,
    pk_ivk_owner: Hash32,
    depth: u8,
    anchor: Hash32,
    inputs: &[SpendInputV2],
    withdraw_amount: u64,
    withdraw_to: Hash32,
    outputs: &[SpendOutputV2],
    blacklist_root: Hash32,
    deny_map_openings: &[DenyMapOpeningV2],
    authority_vfk: Option<Hash32>,
    view_attestations: Option<&[ViewerAttestationV2]>,
) -> Result<(Vec<serde_json::Value>, Vec<usize>)> {
    let depth_usize = depth as usize;
    anyhow::ensure!(!inputs.is_empty(), "note_spend_guest requires at least 1 input");
    anyhow::ensure!(
        inputs.len() <= 4,
        "note_spend_guest supports at most 4 inputs"
    );
    anyhow::ensure!(
        outputs.len() <= 2,
        "note_spend_guest supports at most 2 outputs"
    );

    // Compute inv_enforce from values and rhos (must match the guest's inv_enforce computation).
    let in_values: Vec<u64> = inputs.iter().map(|i| i.value).collect();
    let in_rhos: Vec<Hash32> = inputs.iter().map(|i| i.rho).collect();
    let out_values: Vec<u64> = outputs.iter().map(|o| o.value).collect();
    let out_rhos: Vec<Hash32> = outputs.iter().map(|o| o.rho).collect();
    let inv_enforce = midnight_privacy::inv_enforce_v2(&in_values, &in_rhos, &out_values, &out_rhos);

    // Build args + private indices in the exact order required by note_spend_guest v2.
    let mut args: Vec<serde_json::Value> = Vec::new();
    let mut private_indices: Vec<usize> = Vec::new();
    let mut push = |arg: serde_json::Value, private: bool| {
        args.push(arg);
        if private {
            private_indices.push(args.len()); // 1-based
        }
    };

    // Header:
    push(json!({ "hex": hex32(&domain) }), false); // 1 domain (public)
    push(json!({ "hex": hex32(&spend_sk) }), true); // 2 spend_sk (private)
    push(json!({ "hex": hex32(&pk_ivk_owner) }), true); // 3 pk_ivk_owner (private)
    push(json!({ "i64": depth as i64 }), false); // 4 depth (public)
    push(json!({ "hex": hex32(&anchor) }), false); // 5 anchor (public)
    push(json!({ "i64": inputs.len() as i64 }), false); // 6 n_in (public)

    // Inputs.
    for input in inputs {
        push(
            json!({ "i64": u64_to_i64(input.value, "value_in")? }),
            true,
        );
        push(json!({ "hex": hex32(&input.rho) }), true);
        push(json!({ "hex": hex32(&input.sender_id) }), true);
        // pos_i (private i64; bits derived in-circuit).
        push(json!({ "i64": u64_to_i64(input.pos, "pos")? }), true);

        anyhow::ensure!(
            input.siblings.len() == depth_usize,
            "input Merkle opening has wrong sibling length: got {}, expected {}",
            input.siblings.len(),
            depth_usize
        );
        for sib in &input.siblings {
            push(json!({ "hex": hex32(sib) }), true);
        }

        // Nullifier (public).
        push(json!({ "hex": hex32(&input.nullifier) }), false);
    }

    // Withdraw binding.
    push(
        json!({ "i64": u64_to_i64(withdraw_amount, "withdraw_amount")? }),
        false,
    );
    push(json!({ "hex": hex32(&withdraw_to) }), false);
    push(json!({ "i64": outputs.len() as i64 }), false); // n_out

    // Outputs.
    for out in outputs {
        push(
            json!({ "i64": u64_to_i64(out.value, "value_out")? }),
            true,
        );
        push(json!({ "hex": hex32(&out.rho) }), true);
        push(json!({ "hex": hex32(&out.pk_spend) }), true);
        push(json!({ "hex": hex32(&out.pk_ivk) }), true);
        push(json!({ "hex": hex32(&out.cm) }), false); // cm_out (public)
    }

    // inv_enforce (private).
    push(json!({ "hex": hex32(&inv_enforce) }), true);

    // === Deny-map (blacklist) arguments ===
    //
    // ABI extension (note_spend_guest v2 w/ deny-map buckets):
    //   - blacklist_root (PUBLIC)
    //   - for each checked id:
    //       bucket_entries[BLACKLIST_BUCKET_SIZE] (PRIVATE)
    //       bucket_inv (PRIVATE)
    //       bucket_siblings[BLACKLIST_TREE_DEPTH] (PRIVATE)
    let bl_depth = BLACKLIST_TREE_DEPTH as usize;
    let expected_checks = if withdraw_amount == 0 { 2usize } else { 1usize };
    anyhow::ensure!(
        deny_map_openings.len() == expected_checks,
        "deny-map openings length mismatch: got {}, expected {}",
        deny_map_openings.len(),
        expected_checks
    );

    let pk_spend_owner = midnight_privacy::pk_from_sk(&spend_sk);
    let sender_id = midnight_privacy::recipient_from_pk_v2(&domain, &pk_spend_owner, &pk_ivk_owner);
    let pay_recipient = if withdraw_amount == 0 {
        anyhow::ensure!(!outputs.is_empty(), "transfer must have at least 1 output");
        midnight_privacy::recipient_from_pk_v2(&domain, &outputs[0].pk_spend, &outputs[0].pk_ivk)
    } else {
        [0u8; 32]
    };

    push(json!({ "hex": hex32(&blacklist_root) }), false);
    for (i, opening) in deny_map_openings.iter().enumerate() {
        let id = if i == 0 { sender_id } else { pay_recipient };
        // bucket_entries (private)
        for e in opening.bucket_entries.iter() {
            push(json!({ "hex": hex32(e) }), true);
        }
        // bucket_inv (private)
        let inv = bl_bucket_inv_for_id(&id, &opening.bucket_entries)?;
        push(json!({ "hex": hex32(&inv) }), true);
        // siblings (private)
        anyhow::ensure!(
            opening.siblings.len() == bl_depth,
            "deny-map opening has wrong sibling length: got {}, expected {}",
            opening.siblings.len(),
            bl_depth
        );
        for sib in opening.siblings.iter().take(bl_depth) {
            push(json!({ "hex": hex32(sib) }), true);
        }
    }

    // === Viewer section arguments (Level B) ===
    //
    // If authority VFK is configured, append viewer arguments:
    //   - n_viewers (PUBLIC)
    //   - fvk_commitment (PUBLIC)
    //   - fvk (PRIVATE)
    //   - for each output: ct_hash (PUBLIC), mac (PUBLIC)
    if let (Some(vfk), Some(atts)) = (authority_vfk, view_attestations) {
        let n_out = outputs.len();
        anyhow::ensure!(
            atts.len() >= n_out,
            "viewer attestations length ({}) must be >= number of outputs ({})",
            atts.len(),
            n_out
        );

        // n_viewers (public)
        push(json!({ "i64": 1i64 }), false);
        // fvk_commitment (public)
        let fvk_commitment = atts.first().map(|a| a.fvk_commitment).unwrap_or([0u8; 32]);
        push(json!({ "hex": hex32(&fvk_commitment) }), false);
        // fvk (private)
        push(json!({ "hex": hex32(&vfk) }), true);
        // For each output, ct_hash + mac (public)
        for att in atts.iter().take(n_out) {
            push(json!({ "hex": hex32(&att.ct_hash) }), false);
            push(json!({ "hex": hex32(&att.mac) }), false);
        }
    }

    Ok((args, private_indices))
}
