use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, FromQueryResult, JsonValue,
    QueryFilter, QuerySelect,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::index_db as idx;
use crate::viewer;
use midnight_privacy::{nullifier, recipient_from_pk_v2, EncryptedNote, Hash32, PrivacyAddress};

const DOMAIN: Hash32 = [1u8; 32];
const NULLIFIER_CHUNK_SIZE: usize = 500;
// SQLite commonly defaults to 999 bind parameters; keep `IN (...)` batches below that.
const EVENT_ID_CHUNK_SIZE: usize = 900;

#[derive(Debug, Deserialize, ToSchema)]
pub struct BalanceRequest {
    pub nf_key: String,
    #[serde(default)]
    pub vfk: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BalanceResponse {
    pub balance: String,
    pub unspent_notes: Vec<UnspentNote>,
}

#[derive(Debug, Serialize, ToSchema)]
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
    nullifier: String,
}

pub async fn get_wallet_balance(
    db: &DatabaseConnection,
    address: &str,
    req: BalanceRequest,
) -> Result<BalanceResponse> {
    #[derive(Debug)]
    struct DepositRow {
        event_id: i32,
        amount: Option<String>,
        rho: Option<String>,
        encrypted_notes: Option<JsonValue>,
    }

    #[derive(Debug)]
    struct TransferRow {
        event_id: i32,
        decrypted_notes: Option<JsonValue>,
        encrypted_notes: Option<JsonValue>,
    }

    #[derive(Debug)]
    struct WithdrawRow {
        event_id: i32,
        encrypted_notes: Option<JsonValue>,
    }

    let parsed_address = address
        .parse::<PrivacyAddress>()
        .context("Invalid privacy address")?;
    let nf_key = parse_hash32_hex(&req.nf_key, "nf_key")?;

    let pk_spend = parsed_address.to_pk();
    let pk_ivk = parsed_address.pk_ivk();
    let user_recipient = recipient_from_pk_v2(&DOMAIN, &pk_spend, &pk_ivk);

    let vfk = match req.vfk {
        Some(vfk_hex) => Some(parse_hash32_hex(&vfk_hex, "vfk")?),
        None => None,
    };

    // Convert the wallet recipient hash to bech32m for querying deposits/transfers by involvement.
    let wallet_bech32m = viewer::hex_to_bech32m_address(&hex::encode(user_recipient))
        .context("Failed to convert wallet recipient to bech32m")?;

    let deposit_rows: Vec<DepositRow> = if vfk.is_some() {
        #[derive(FromQueryResult)]
        struct DepositDbRow {
            event_id: i32,
            amount: Option<String>,
            rho: Option<String>,
            encrypted_notes: Option<JsonValue>,
        }

        idx::midnight_deposit::Entity::find()
            .select_only()
            .column(idx::midnight_deposit::Column::EventId)
            .column(idx::midnight_deposit::Column::Amount)
            .column(idx::midnight_deposit::Column::Rho)
            .column(idx::midnight_deposit::Column::EncryptedNotes)
            .filter(idx::midnight_deposit::Column::Recipient.eq(wallet_bech32m.clone()))
            .into_model::<DepositDbRow>()
            .all(db)
            .await?
            .into_iter()
            .map(|r| DepositRow {
                event_id: r.event_id,
                amount: r.amount,
                rho: r.rho,
                encrypted_notes: r.encrypted_notes,
            })
            .collect()
    } else {
        #[derive(FromQueryResult)]
        struct DepositDbRow {
            event_id: i32,
            amount: Option<String>,
            rho: Option<String>,
        }

        idx::midnight_deposit::Entity::find()
            .select_only()
            .column(idx::midnight_deposit::Column::EventId)
            .column(idx::midnight_deposit::Column::Amount)
            .column(idx::midnight_deposit::Column::Rho)
            .filter(idx::midnight_deposit::Column::Recipient.eq(wallet_bech32m.clone()))
            .filter(idx::midnight_deposit::Column::Amount.is_not_null())
            .filter(idx::midnight_deposit::Column::Rho.is_not_null())
            .into_model::<DepositDbRow>()
            .all(db)
            .await?
            .into_iter()
            .map(|r| DepositRow {
                event_id: r.event_id,
                amount: r.amount,
                rho: r.rho,
                encrypted_notes: None,
            })
            .collect()
    };

    // Transfers:
    // - Prefer indexed involvement fields (`recipient`, `privacy_sender`) to keep this query scoped
    //   to the wallet and avoid scanning the entire transfer table.
    // - When a VFK is provided, also include *untagged* transfers (missing recipient/privacy_sender)
    //   that still have encrypted notes, so we can decrypt and recover change notes.
    // - Without a VFK, include only a small backward-compatibility set: rows with decrypted notes
    //   but missing involvement fields.
    let missing_involvement_fields = Condition::all()
        .add(idx::midnight_transfer::Column::Recipient.is_null())
        .add(idx::midnight_transfer::Column::PrivacySender.is_null());
    let transfer_filter = if vfk.is_some() {
        Condition::any()
            .add(idx::midnight_transfer::Column::Recipient.eq(wallet_bech32m.clone()))
            .add(idx::midnight_transfer::Column::PrivacySender.eq(wallet_bech32m.clone()))
            .add(
                Condition::all()
                    .add(idx::midnight_transfer::Column::EncryptedNotes.is_not_null())
                    .add(missing_involvement_fields.clone()),
            )
    } else {
        Condition::any()
            .add(idx::midnight_transfer::Column::Recipient.eq(wallet_bech32m.clone()))
            .add(idx::midnight_transfer::Column::PrivacySender.eq(wallet_bech32m.clone()))
            .add(
                Condition::all()
                    .add(idx::midnight_transfer::Column::DecryptedNotes.is_not_null())
                    .add(missing_involvement_fields.clone()),
            )
    };
    let transfer_rows: Vec<TransferRow> = if vfk.is_some() {
        #[derive(FromQueryResult)]
        struct TransferDbRow {
            event_id: i32,
            decrypted_notes: Option<JsonValue>,
            encrypted_notes: Option<JsonValue>,
        }

        idx::midnight_transfer::Entity::find()
            .select_only()
            .column(idx::midnight_transfer::Column::EventId)
            .column(idx::midnight_transfer::Column::DecryptedNotes)
            .column(idx::midnight_transfer::Column::EncryptedNotes)
            .filter(transfer_filter)
            .into_model::<TransferDbRow>()
            .all(db)
            .await?
            .into_iter()
            .map(|r| TransferRow {
                event_id: r.event_id,
                decrypted_notes: r.decrypted_notes,
                encrypted_notes: r.encrypted_notes,
            })
            .collect()
    } else {
        #[derive(FromQueryResult)]
        struct TransferDbRow {
            event_id: i32,
            decrypted_notes: Option<JsonValue>,
        }

        idx::midnight_transfer::Entity::find()
            .select_only()
            .column(idx::midnight_transfer::Column::EventId)
            .column(idx::midnight_transfer::Column::DecryptedNotes)
            .filter(idx::midnight_transfer::Column::DecryptedNotes.is_not_null())
            .filter(transfer_filter)
            .into_model::<TransferDbRow>()
            .all(db)
            .await?
            .into_iter()
            .map(|r| TransferRow {
                event_id: r.event_id,
                decrypted_notes: r.decrypted_notes,
                encrypted_notes: None,
            })
            .collect()
    };

    let withdraw_rows: Vec<WithdrawRow> = if vfk.is_some() {
        #[derive(FromQueryResult)]
        struct WithdrawDbRow {
            event_id: i32,
            encrypted_notes: Option<JsonValue>,
        }

        let withdraw_filter = Condition::any()
            .add(idx::midnight_withdraw::Column::PrivacySender.eq(wallet_bech32m.clone()))
            // Backward-compat: include untagged withdraws so we can decrypt and recover
            // change notes even if `privacy_sender` hasn't been backfilled yet.
            .add(idx::midnight_withdraw::Column::PrivacySender.is_null());

        idx::midnight_withdraw::Entity::find()
            .select_only()
            .column(idx::midnight_withdraw::Column::EventId)
            .column(idx::midnight_withdraw::Column::EncryptedNotes)
            .filter(idx::midnight_withdraw::Column::EncryptedNotes.is_not_null())
            .filter(withdraw_filter)
            .into_model::<WithdrawDbRow>()
            .all(db)
            .await?
            .into_iter()
            .map(|r| WithdrawRow {
                event_id: r.event_id,
                encrypted_notes: r.encrypted_notes,
            })
            .collect()
    } else {
        Vec::new()
    };

    let mut event_ids = Vec::new();
    event_ids.extend(deposit_rows.iter().map(|row| row.event_id));
    event_ids.extend(transfer_rows.iter().map(|row| row.event_id));
    event_ids.extend(withdraw_rows.iter().map(|row| row.event_id));

    let event_map = load_event_map(db, &event_ids).await?;

    let mut notes = Vec::new();
    let mut seen_rhos = HashSet::new();

    for row in deposit_rows {
        let Some((tx_hash, timestamp_ms)) = event_map.get(&row.event_id) else {
            continue;
        };
        if let (Some(amount), Some(rho_hex)) = (row.amount.as_ref(), row.rho.as_ref()) {
            if let (Ok(value), Ok(rho)) = (amount.parse::<u128>(), parse_hash32_hex(rho_hex, "rho"))
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

        let decrypted_notes = notes_from_row(row.encrypted_notes.as_ref(), vfk.as_ref());
        for note in decrypted_notes {
            if let Some(record) =
                note_from_decrypted(&note, &user_recipient, tx_hash, *timestamp_ms, "deposit")
            {
                add_note(&mut notes, &mut seen_rhos, record);
            }
        }
    }

    for row in transfer_rows {
        let Some((tx_hash, timestamp_ms)) = event_map.get(&row.event_id) else {
            continue;
        };
        let mut decrypted_notes = notes_from_decrypted_row(row.decrypted_notes.as_ref());
        if decrypted_notes.is_empty() {
            decrypted_notes = notes_from_row(row.encrypted_notes.as_ref(), vfk.as_ref());
        }
        for note in decrypted_notes {
            if let Some(record) =
                note_from_decrypted(&note, &user_recipient, tx_hash, *timestamp_ms, "transfer")
            {
                add_note(&mut notes, &mut seen_rhos, record);
            }
        }
    }

    for row in withdraw_rows {
        let Some((tx_hash, timestamp_ms)) = event_map.get(&row.event_id) else {
            continue;
        };
        let decrypted_notes = notes_from_row(row.encrypted_notes.as_ref(), vfk.as_ref());
        for note in decrypted_notes {
            if let Some(record) =
                note_from_decrypted(&note, &user_recipient, tx_hash, *timestamp_ms, "withdraw")
            {
                add_note(&mut notes, &mut seen_rhos, record);
            }
        }
    }

    let mut note_states = Vec::new();
    let mut nullifier_lookup = HashSet::new();
    for note in notes {
        let nf = nullifier(&DOMAIN, &nf_key, &note.rho);
        let nf_hex = hex::encode(nf);

        nullifier_lookup.insert(nf_hex.clone());
        nullifier_lookup.insert(format!("0x{}", nf_hex));

        note_states.push(NoteState {
            note,
            nullifier: nf_hex,
        });
    }

    let spent_nullifiers = fetch_spent_nullifiers(db, &nullifier_lookup).await?;

    let mut unspent_notes = Vec::new();
    let mut balance: u128 = 0;

    for state in note_states {
        if spent_nullifiers.contains(&state.nullifier) {
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
    event_ids: &[i32],
) -> Result<HashMap<i32, (String, i64)>> {
    #[derive(FromQueryResult)]
    struct EventMetaRow {
        id: i32,
        tx_hash: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }

    let mut event_ids = event_ids.to_vec();
    event_ids.sort_unstable();
    event_ids.dedup();

    if event_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Only fetch the event columns we need for balance computation. Avoid selecting
    // large `payload`/`events` JSON blobs to reduce DB CPU + network I/O.
    let mut map = HashMap::with_capacity(event_ids.len());
    for chunk in event_ids.chunks(EVENT_ID_CHUNK_SIZE) {
        let events: Vec<EventMetaRow> = idx::Entity::find()
            .select_only()
            .column(idx::Column::Id)
            .column(idx::Column::TxHash)
            .column(idx::Column::CreatedAt)
            .filter(idx::Column::Id.is_in(chunk.to_vec()))
            .into_model::<EventMetaRow>()
            .all(db)
            .await?;

        for ev in events {
            map.insert(ev.id, (ev.tx_hash, ev.created_at.timestamp_millis()));
        }
    }
    Ok(map)
}

fn add_note(notes: &mut Vec<NoteRecord>, seen_rhos: &mut HashSet<Hash32>, note: NoteRecord) {
    if seen_rhos.insert(note.rho) {
        notes.push(note);
    }
}

fn notes_from_row(
    encrypted: Option<&serde_json::Value>,
    vfk: Option<&Hash32>,
) -> Vec<viewer::DecryptedNote> {
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

fn notes_from_decrypted_row(decrypted: Option<&serde_json::Value>) -> Vec<viewer::DecryptedNote> {
    let Some(json) = decrypted else {
        return Vec::new();
    };

    serde_json::from_value(json.clone()).unwrap_or_default()
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
    #[derive(FromQueryResult)]
    struct SpentNullifierRow {
        nullifier: String,
    }

    let mut spent = HashSet::new();
    if nullifiers.is_empty() {
        return Ok(spent);
    }

    let mut values: Vec<String> = nullifiers.iter().cloned().collect();
    values.sort();

    for chunk in values.chunks(NULLIFIER_CHUNK_SIZE) {
        let chunk_vec: Vec<String> = chunk.to_vec();
        let rows: Vec<SpentNullifierRow> = idx::midnight_spent_nullifiers::Entity::find()
            .select_only()
            .column(idx::midnight_spent_nullifiers::Column::Nullifier)
            .filter(idx::midnight_spent_nullifiers::Column::Nullifier.is_in(chunk_vec))
            .into_model::<SpentNullifierRow>()
            .all(db)
            .await?;
        for row in rows {
            spent.insert(normalize_nullifier(&row.nullifier));
        }
    }

    Ok(spent)
}

fn normalize_nullifier(value: &str) -> String {
    value
        .trim()
        .strip_prefix("0x")
        .unwrap_or(value)
        .to_lowercase()
}

fn parse_hash32_hex(value: &str, field: &str) -> Result<Hash32> {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(trimmed).with_context(|| format!("Invalid hex for {field}"))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "Expected 32-byte hex for {field}, got {} bytes",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}
