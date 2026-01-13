use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::index_db as idx;
use crate::viewer;
use midnight_privacy::{
    nullifier, nf_key_from_sk, pk_from_sk, pk_ivk_from_sk, recipient_from_pk_v2, EncryptedNote,
    Hash32, PrivacyAddress,
};

const DOMAIN: Hash32 = [1u8; 32];
const LEGACY_NF_KEY: Hash32 = [4u8; 32];
const NULLIFIER_CHUNK_SIZE: usize = 500;

#[derive(Debug, Deserialize)]
pub struct BalanceRequest {
    pub spend_sk: String,
    #[serde(default)]
    pub vfk: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BalanceResponse {
    pub balance: String,
    pub unspent_notes: Vec<UnspentNote>,
}

#[derive(Debug, Serialize)]
pub struct UnspentNote {
    pub value: String,
    pub rho: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    pub tx_hash: String,
    pub timestamp_ms: i64,
    pub kind: String,
}

struct NoteRecord {
    rho: Hash32,
    value: u128,
    sender_id: Option<Hash32>,
    tx_hash: String,
    timestamp_ms: i64,
    kind: String,
}

struct NoteState {
    note: NoteRecord,
    nullifier_user: String,
    nullifier_legacy: String,
}

pub async fn get_wallet_balance(
    db: &DatabaseConnection,
    address: &str,
    req: BalanceRequest,
) -> Result<BalanceResponse> {
    let spend_sk = parse_hash32_hex(&req.spend_sk, "spend_sk")?;
    let pk_spend = pk_from_sk(&spend_sk);
    let pk_ivk = pk_ivk_from_sk(&DOMAIN, &spend_sk);
    let derived_address = PrivacyAddress::from_keys(&pk_spend, &pk_ivk).to_string();

    let normalized_address = address
        .parse::<PrivacyAddress>()
        .context("Invalid privacy address")?
        .to_string();
    if derived_address != normalized_address {
        anyhow::bail!(
            "Provided spend_sk does not match address {}",
            address
        );
    }
    let address = normalized_address;

    let user_recipient = recipient_from_pk_v2(&DOMAIN, &pk_spend, &pk_ivk);
    let nf_key = nf_key_from_sk(&DOMAIN, &spend_sk);
    let vfk = match req.vfk {
        Some(vfk_hex) => Some(parse_hash32_hex(&vfk_hex, "vfk")?),
        None => None,
    };

    // Convert recipient hash to bech32m for querying deposits
    let recipient_bech32m = viewer::hex_to_bech32m_address(&hex::encode(user_recipient))
        .context("Failed to convert recipient to bech32m")?;

    let deposit_rows = idx::midnight_deposit::Entity::find()
        .filter(idx::midnight_deposit::Column::Recipient.eq(recipient_bech32m.clone()))
        .all(db)
        .await?;

    // When VFK is provided, fetch ALL transfers with encrypted notes so we can decrypt
    // and find change notes from outgoing transfers. Otherwise, only fetch transfers
    // where we are the recipient (based on the stored recipient field).
    let transfer_filter = if vfk.is_some() {
        Condition::any()
            .add(idx::midnight_transfer::Column::Recipient.eq(recipient_bech32m.clone()))
            .add(idx::midnight_transfer::Column::EncryptedNotes.is_not_null())
    } else {
        Condition::any().add(idx::midnight_transfer::Column::Recipient.eq(recipient_bech32m.clone()))
    };
    let transfer_rows = idx::midnight_transfer::Entity::find()
        .filter(transfer_filter)
        .all(db)
        .await?;

    let withdraw_filter = if vfk.is_some() {
        Condition::any()
            .add(idx::midnight_withdraw::Column::DecryptedNotes.is_not_null())
            .add(idx::midnight_withdraw::Column::EncryptedNotes.is_not_null())
    } else {
        Condition::any().add(idx::midnight_withdraw::Column::DecryptedNotes.is_not_null())
    };
    let withdraw_rows = idx::midnight_withdraw::Entity::find()
        .filter(withdraw_filter)
        .all(db)
        .await?;

    let event_map = load_event_map(db, &deposit_rows, &transfer_rows, &withdraw_rows).await?;

    let mut notes = Vec::new();
    let mut seen_rhos = HashSet::new();

    for row in deposit_rows {
        let Some((tx_hash, timestamp_ms)) = event_map.get(&row.event_id) else {
            continue;
        };
        if let (Some(amount), Some(rho_hex)) = (row.amount.as_ref(), row.rho.as_ref()) {
            if let (Ok(value), Ok(rho)) =
                (amount.parse::<u128>(), parse_hash32_hex(rho_hex, "rho"))
            {
                add_note(
                    &mut notes,
                    &mut seen_rhos,
                    NoteRecord {
                        rho,
                        value,
                        sender_id: None,
                        tx_hash: tx_hash.clone(),
                        timestamp_ms: *timestamp_ms,
                        kind: "deposit".to_string(),
                    },
                );
            }
        }

        let decrypted_notes = notes_from_row(
            row.decrypted_notes.as_ref(),
            row.encrypted_notes.as_ref(),
            vfk.as_ref(),
        );
        for note in decrypted_notes {
            if let Some(record) = note_from_decrypted(
                &note,
                &user_recipient,
                tx_hash,
                *timestamp_ms,
                "deposit",
            ) {
                add_note(&mut notes, &mut seen_rhos, record);
            }
        }
    }

    for row in transfer_rows {
        let Some((tx_hash, timestamp_ms)) = event_map.get(&row.event_id) else {
            continue;
        };
        let decrypted_notes = notes_from_row(
            row.decrypted_notes.as_ref(),
            row.encrypted_notes.as_ref(),
            vfk.as_ref(),
        );
        for note in decrypted_notes {
            if let Some(record) = note_from_decrypted(
                &note,
                &user_recipient,
                tx_hash,
                *timestamp_ms,
                "transfer",
            ) {
                add_note(&mut notes, &mut seen_rhos, record);
            }
        }
    }

    for row in withdraw_rows {
        let Some((tx_hash, timestamp_ms)) = event_map.get(&row.event_id) else {
            continue;
        };
        let decrypted_notes = notes_from_row(
            row.decrypted_notes.as_ref(),
            row.encrypted_notes.as_ref(),
            vfk.as_ref(),
        );
        for note in decrypted_notes {
            if let Some(record) = note_from_decrypted(
                &note,
                &user_recipient,
                tx_hash,
                *timestamp_ms,
                "withdraw",
            ) {
                add_note(&mut notes, &mut seen_rhos, record);
            }
        }
    }

    let mut note_states = Vec::new();
    let mut nullifier_lookup = HashSet::new();
    for note in notes {
        let nf_user = nullifier(&DOMAIN, &nf_key, &note.rho);
        let nf_legacy = nullifier(&DOMAIN, &LEGACY_NF_KEY, &note.rho);
        let nf_user_hex = hex::encode(nf_user);
        let nf_legacy_hex = hex::encode(nf_legacy);

        nullifier_lookup.insert(nf_user_hex.clone());
        nullifier_lookup.insert(format!("0x{}", nf_user_hex));
        nullifier_lookup.insert(nf_legacy_hex.clone());
        nullifier_lookup.insert(format!("0x{}", nf_legacy_hex));

        note_states.push(NoteState {
            note,
            nullifier_user: nf_user_hex,
            nullifier_legacy: nf_legacy_hex,
        });
    }

    let spent_nullifiers = fetch_spent_nullifiers(db, &nullifier_lookup).await?;

    let mut unspent_notes = Vec::new();
    let mut balance: u128 = 0;

    for state in note_states {
        let spent_by_user = spent_nullifiers.contains(&state.nullifier_user);
        let spent_by_legacy = spent_nullifiers.contains(&state.nullifier_legacy);

        if spent_by_user || spent_by_legacy {
            continue;
        }

        balance = balance.saturating_add(state.note.value);
        unspent_notes.push(UnspentNote {
            value: state.note.value.to_string(),
            rho: hex::encode(state.note.rho),
            sender_id: state.note.sender_id.map(hex::encode),
            tx_hash: state.note.tx_hash,
            timestamp_ms: state.note.timestamp_ms,
            kind: state.note.kind,
        });
    }

    unspent_notes.sort_by(|a, b| {
        b.timestamp_ms
            .cmp(&a.timestamp_ms)
            .then(b.tx_hash.cmp(&a.tx_hash))
    });

    Ok(BalanceResponse {
        balance: balance.to_string(),
        unspent_notes,
    })
}

async fn load_event_map(
    db: &DatabaseConnection,
    deposits: &[idx::midnight_deposit::Model],
    transfers: &[idx::midnight_transfer::Model],
    withdraws: &[idx::midnight_withdraw::Model],
) -> Result<HashMap<i32, (String, i64)>> {
    let mut event_ids = Vec::new();
    event_ids.extend(deposits.iter().map(|row| row.event_id));
    event_ids.extend(transfers.iter().map(|row| row.event_id));
    event_ids.extend(withdraws.iter().map(|row| row.event_id));
    event_ids.sort_unstable();
    event_ids.dedup();

    if event_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let events = idx::Entity::find()
        .filter(idx::Column::Id.is_in(event_ids))
        .all(db)
        .await?;

    let mut map = HashMap::new();
    for ev in events {
        map.insert(ev.id, (ev.tx_hash, ev.created_at.timestamp_millis()));
    }
    Ok(map)
}

fn add_note(notes: &mut Vec<NoteRecord>, seen_rhos: &mut HashSet<Hash32>, note: NoteRecord) {
    if seen_rhos.insert(note.rho) {
        notes.push(note);
    }
}

fn notes_from_row(
    decrypted: Option<&serde_json::Value>,
    encrypted: Option<&serde_json::Value>,
    vfk: Option<&Hash32>,
) -> Vec<viewer::DecryptedNote> {
    if let Some(json) = decrypted {
        return serde_json::from_value::<Vec<viewer::DecryptedNote>>(json.clone()).unwrap_or_default();
    }

    let Some(vfk) = vfk else {
        return Vec::new();
    };
    let Some(json) = encrypted else {
        return Vec::new();
    };

    let notes: Vec<EncryptedNote> = serde_json::from_value(json.clone()).unwrap_or_default();
    let mut decrypted_notes = Vec::new();
    for note in notes {
        if let Ok(decrypted) = viewer::decrypt_note(vfk, &note) {
            decrypted_notes.push(decrypted);
        }
    }
    decrypted_notes
}

fn note_from_decrypted(
    note: &viewer::DecryptedNote,
    user_recipient: &Hash32,
    tx_hash: &str,
    timestamp_ms: i64,
    kind: &str,
) -> Option<NoteRecord> {
    let recipient = parse_hash32_hex(&note.recipient, "recipient").ok()?;
    if &recipient != user_recipient {
        return None;
    }

    let rho = parse_hash32_hex(&note.rho, "rho").ok()?;
    let value = note.value.parse::<u128>().ok()?;
    let sender_id = note
        .sender_id
        .as_ref()
        .and_then(|value| parse_hash32_hex(value, "sender_id").ok());

    Some(NoteRecord {
        rho,
        value,
        sender_id,
        tx_hash: tx_hash.to_string(),
        timestamp_ms,
        kind: kind.to_string(),
    })
}

async fn fetch_spent_nullifiers(
    db: &DatabaseConnection,
    nullifiers: &HashSet<String>,
) -> Result<HashSet<String>> {
    let mut spent = HashSet::new();
    if nullifiers.is_empty() {
        return Ok(spent);
    }

    let mut values: Vec<String> = nullifiers.iter().cloned().collect();
    values.sort();

    for chunk in values.chunks(NULLIFIER_CHUNK_SIZE) {
        let chunk_vec: Vec<String> = chunk.to_vec();
        let transfer_rows = idx::midnight_transfer::Entity::find()
            .filter(idx::midnight_transfer::Column::Nullifier.is_in(chunk_vec.clone()))
            .all(db)
            .await?;
        for row in transfer_rows {
            if let Some(nullifier) = row.nullifier {
                spent.insert(normalize_nullifier(&nullifier));
            }
        }

        let withdraw_rows = idx::midnight_withdraw::Entity::find()
            .filter(idx::midnight_withdraw::Column::Nullifier.is_in(chunk_vec))
            .all(db)
            .await?;
        for row in withdraw_rows {
            if let Some(nullifier) = row.nullifier {
                spent.insert(normalize_nullifier(&nullifier));
            }
        }
    }

    Ok(spent)
}

fn normalize_nullifier(value: &str) -> String {
    value.trim().strip_prefix("0x").unwrap_or(value).to_lowercase()
}

fn parse_hash32_hex(value: &str, field: &str) -> Result<Hash32> {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes =
        hex::decode(trimmed).with_context(|| format!("Invalid hex for {field}"))?;
    if bytes.len() != 32 {
        anyhow::bail!("Expected 32-byte hex for {field}, got {} bytes", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}
