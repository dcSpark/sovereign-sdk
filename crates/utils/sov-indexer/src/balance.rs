use std::collections::HashSet;
use std::time::Instant;

use anyhow::{Context, Result};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::index_db as idx;
use crate::viewer;
use midnight_privacy::{nullifier, recipient_from_pk_v2, Hash32, PrivacyAddress};

const DOMAIN: Hash32 = [1u8; 32];
const NULLIFIER_CHUNK_SIZE: usize = 500;

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
    let parsed_address = address
        .parse::<PrivacyAddress>()
        .context("Invalid privacy address")?;
    let nf_key = parse_hash32_hex(&req.nf_key, "nf_key")?;

    let pk_spend = parsed_address.to_pk();
    let pk_ivk = parsed_address.pk_ivk();
    let user_recipient = recipient_from_pk_v2(&DOMAIN, &pk_spend, &pk_ivk);

    // Keep request validation behavior compatible even though vfk is not needed in notes index mode.
    let _vfk = match req.vfk {
        Some(vfk_hex) => Some(parse_hash32_hex(&vfk_hex, "vfk")?),
        None => None,
    };

    // Convert the wallet recipient hash to bech32m for querying deposits/transfers by involvement.
    let wallet_bech32m = viewer::hex_to_bech32m_address(&hex::encode(user_recipient))
        .context("Failed to convert wallet recipient to bech32m")?;

    let started = Instant::now();
    let response = get_wallet_balance_from_notes_index(db, &wallet_bech32m, &nf_key).await?;
    tracing::debug!(
        address,
        source = "notes_nullifiers",
        elapsed_ms = started.elapsed().as_millis(),
        note_count = response.unspent_notes.len(),
        "Computed wallet balance"
    );
    Ok(response)
}

async fn get_wallet_balance_from_notes_index(
    db: &DatabaseConnection,
    wallet_bech32m: &str,
    nf_key: &Hash32,
) -> Result<BalanceResponse> {
    #[derive(FromQueryResult)]
    struct NotesNullifierRow {
        rho: Option<String>,
        value: Option<String>,
        sender_id: Option<String>,
        created_tx_hash: Option<String>,
        created_at: Option<chrono::DateTime<chrono::Utc>>,
        created_kind: Option<String>,
    }

    let rows: Vec<NotesNullifierRow> = match idx::notes_nullifiers::Entity::find()
        .select_only()
        .column(idx::notes_nullifiers::Column::Rho)
        .column(idx::notes_nullifiers::Column::Value)
        .column(idx::notes_nullifiers::Column::SenderId)
        .column(idx::notes_nullifiers::Column::CreatedTxHash)
        .column(idx::notes_nullifiers::Column::CreatedAt)
        .column(idx::notes_nullifiers::Column::CreatedKind)
        .filter(idx::notes_nullifiers::Column::Recipient.eq(wallet_bech32m.to_string()))
        .filter(idx::notes_nullifiers::Column::Rho.is_not_null())
        .filter(idx::notes_nullifiers::Column::Value.is_not_null())
        // Filter obvious spends early; we still verify with nullifier lookups to
        // tolerate temporary backfill lag in notes_nullifiers.
        .filter(idx::notes_nullifiers::Column::SpentTxHash.is_null())
        .into_model::<NotesNullifierRow>()
        .all(db)
        .await
    {
        Ok(rows) => rows,
        Err(err) if is_missing_notes_index_table(&err) => {
            return Err(anyhow::anyhow!(
                "notes_nullifiers table is required for wallet balance but is unavailable: {}",
                err
            ));
        }
        Err(err) => return Err(err.into()),
    };

    if rows.is_empty() {
        return Ok(BalanceResponse {
            balance: "0".to_string(),
            unspent_notes: Vec::new(),
        });
    }

    let mut notes = Vec::with_capacity(rows.len());
    let mut seen_rhos = HashSet::with_capacity(rows.len());
    for row in rows {
        let (Some(rho_hex), Some(value_str)) = (row.rho.as_deref(), row.value.as_deref()) else {
            continue;
        };
        let Ok(rho) = parse_hash32_hex(rho_hex, "rho") else {
            continue;
        };
        let Ok(value) = value_str.parse::<u128>() else {
            continue;
        };
        let sender_id = row.sender_id.as_deref().and_then(parse_sender_id_to_hash32);

        add_note(
            &mut notes,
            &mut seen_rhos,
            NoteRecord {
                rho,
                value,
                sender_id,
                tx_hash: row.created_tx_hash.unwrap_or_default(),
                timestamp_ms: row
                    .created_at
                    .map(|ts| ts.timestamp_millis())
                    .unwrap_or_default(),
                kind: row.created_kind.unwrap_or_else(|| "transfer".to_string()),
            },
        );
    }

    if notes.is_empty() {
        return Ok(BalanceResponse {
            balance: "0".to_string(),
            unspent_notes: Vec::new(),
        });
    }

    finalize_balance_from_notes(db, notes, nf_key).await
}

async fn finalize_balance_from_notes(
    db: &DatabaseConnection,
    notes: Vec<NoteRecord>,
    nf_key: &Hash32,
) -> Result<BalanceResponse> {
    let mut note_states = Vec::with_capacity(notes.len());
    let mut nullifier_lookup = HashSet::with_capacity(notes.len().saturating_mul(2));
    for note in notes {
        let nf = nullifier(&DOMAIN, nf_key, &note.rho);
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

fn add_note(notes: &mut Vec<NoteRecord>, seen_rhos: &mut HashSet<Hash32>, note: NoteRecord) {
    if seen_rhos.insert(note.rho) {
        notes.push(note);
    }
}

fn parse_sender_id_to_hash32(value: &str) -> Option<Hash32> {
    if let Ok(privacy_address) = value.parse::<PrivacyAddress>() {
        let pk_spend = privacy_address.to_pk();
        let pk_ivk = privacy_address.pk_ivk();
        return Some(recipient_from_pk_v2(&DOMAIN, &pk_spend, &pk_ivk));
    }

    parse_hash32_hex(value, "sender_id").ok()
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

fn is_missing_notes_index_table(err: &sea_orm::DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("notes_nullifiers")
        && (msg.contains("no such table")
            || msg.contains("does not exist")
            || msg.contains("no such column"))
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
