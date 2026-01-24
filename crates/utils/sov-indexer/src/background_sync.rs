use crate::db;
use crate::db::{extract_events_from_status, extract_status_from_status};
use crate::index_db as idx;
use crate::viewer::{
    self, extract_amount_from_decrypted_notes, extract_recipient_from_decrypted_notes,
    extract_sender_from_decrypted_notes, hex_to_bech32m_address, FvkRegistry,
};
use anyhow::Result;
use midnight_privacy::{note_commitment, Hash32};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use sov_midnight_da::storable::worker_verified_transactions;
use sov_midnight_da::storable::worker_verified_transactions::TransactionState as VerifiedState;
use std::sync::Arc;

const PRIVACY_DOMAIN: Hash32 = [1u8; 32];
const PRIVACY_DOMAIN_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

fn is_zero_hash32(hex_str: &str) -> bool {
    let trimmed = hex_str.trim().strip_prefix("0x").unwrap_or(hex_str).to_lowercase();
    trimmed == "0".repeat(64)
}

fn hash32_from_hex(hex_str: &str) -> Option<Hash32> {
    let s = hex_str.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

fn hash32_from_bech32m(value: &str) -> Option<Hash32> {
    let (hrp, bytes) = bech32::decode(value).ok()?;
    if hrp.as_str() != viewer::PRIVACY_ADDRESS_HRP {
        return None;
    }
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

fn compute_deposit_commitment_fallback(
    amount: Option<&str>,
    rho: Option<&str>,
    recipient: Option<&str>,
) -> Option<String> {
    let amount = amount?;
    let rho = rho?;
    let recipient = recipient?;

    let value_u128 = amount.parse::<u128>().ok()?;
    let value_u64 = u64::try_from(value_u128).ok()?;
    let rho = hash32_from_hex(rho)?;

    // `recipient` is stored as bech32m (`privpool1...`) in indexer tables, but accept hex too.
    let recipient_hash = if recipient
        .trim()
        .to_ascii_lowercase()
        .starts_with(&format!("{}1", viewer::PRIVACY_ADDRESS_HRP))
    {
        hash32_from_bech32m(recipient)?
    } else {
        hash32_from_hex(recipient)?
    };

    // Deposit commitment binds `sender_id = recipient` (see module `deposit()` implementation).
    let cm = note_commitment(
        &PRIVACY_DOMAIN,
        value_u64,
        &rho,
        &recipient_hash,
        &recipient_hash,
    );
    Some(hex::encode(cm))
}

fn decrypted_notes_from_json(json: Option<&serde_json::Value>) -> Vec<viewer::DecryptedNote> {
    json.and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn commitment_from_u8_array(value: &serde_json::Value) -> Option<String> {
    let arr = value.as_array()?;
    if arr.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, b) in arr.iter().enumerate() {
        bytes[i] = b.as_u64()? as u8;
    }
    Some(hex::encode(bytes))
}

fn extract_commitment_from_events(
    events: Option<&serde_json::Value>,
    key_suffix: &str,
    value_key: &str,
) -> Option<String> {
    let events = events?;
    let arr = events.as_array()?;
    for ev in arr {
        let key = ev.get("key").and_then(|v| v.as_str())?;
        if !key.ends_with(key_suffix) {
            continue;
        }
        let value = ev.get("value")?;
        let obj = value.as_object()?;
        let inner = obj.get(value_key)?.as_object()?;
        let commitment = inner.get("commitment")?;
        if let Some(hex_str) = commitment.as_str() {
            return Some(hex_str.trim().trim_start_matches("0x").to_ascii_lowercase());
        }
        if let Some(hex_str) = commitment_from_u8_array(commitment) {
            return Some(hex_str);
        }
    }
    None
}

fn extract_deposit_commitment(events: Option<&serde_json::Value>) -> Option<String> {
    // Prefer the dedicated PoolDeposit event (clearly identifies deposit-created notes).
    extract_commitment_from_events(events, "/PoolDeposit", "pool_deposit")
        // Fallback: NoteCreated also carries the commitment.
        .or_else(|| extract_commitment_from_events(events, "/NoteCreated", "note_created"))
}

async fn index_output_notes(
    idx_db: &DatabaseConnection,
    tx_hash: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    kind: &str,
    decrypted: &[viewer::DecryptedNote],
) -> Result<()> {
    for note in decrypted {
        let Some(cm) = note.cm.as_deref() else {
            continue;
        };
        let cm_ins_json = note.cm_ins.as_ref().and_then(|arr| {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|cm_in| !is_zero_hash32(cm_in))
                .cloned()
                .map(serde_json::Value::String)
                .collect();
            if filtered.is_empty() {
                None
            } else {
                Some(serde_json::Value::Array(filtered))
            }
        });
        db::upsert_note_created(
            idx_db,
            cm,
            Some(&note.domain),
            Some(&note.value),
            Some(&note.rho),
            Some(&note.recipient),
            note.sender_id.as_deref(),
            cm_ins_json,
            Some(tx_hash),
            Some(created_at),
            Some(kind),
        )
        .await?;
    }
    Ok(())
}

async fn index_output_note_metadata(
    idx_db: &DatabaseConnection,
    tx_hash: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    kind: &str,
    encrypted_notes: Option<&serde_json::Value>,
) -> Result<()> {
    let Some(arr) = encrypted_notes.and_then(|v| v.as_array()) else {
        return Ok(());
    };
    for note in arr {
        let Some(cm) = note.get("cm").and_then(|v| v.as_str()) else {
            continue;
        };
        db::upsert_note_created_metadata(idx_db, cm, tx_hash, created_at, kind).await?;
    }
    Ok(())
}

async fn index_spent_inputs(
    idx_db: &DatabaseConnection,
    tx_hash: &str,
    spent_at: chrono::DateTime<chrono::Utc>,
    kind: &str,
    spent_nullifiers: Option<&[String]>,
    decrypted: &[viewer::DecryptedNote],
) -> Result<()> {
    // Map each input commitment -> the corresponding spent nullifier (by input index).
    //
    // The v2 plaintext includes cm_ins[4] padded with zeros; we align by index with the
    // transaction's public `nullifiers` list.
    let mut cm_to_nf: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for note in decrypted {
        let Some(cm_ins) = note.cm_ins.as_ref() else {
            continue;
        };
        for (idx, cm_in) in cm_ins.iter().enumerate() {
            if is_zero_hash32(cm_in) {
                continue;
            }
            let nf = spent_nullifiers
                .and_then(|nfs| nfs.get(idx))
                .map(|s| s.clone());

            match cm_to_nf.entry(cm_in.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(nf);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let existing = e.get().as_deref();
                    let incoming = nf.as_deref();
                    if existing.is_some() && incoming.is_some() && existing != incoming {
                        tracing::warn!(
                            tx_hash,
                            kind,
                            cm_in,
                            existing = existing.unwrap(),
                            incoming = incoming.unwrap(),
                            "Conflicting nullifier mapping for cm_in; keeping existing"
                        );
                        continue;
                    }
                    if existing.is_none() && incoming.is_some() {
                        e.insert(nf);
                    }
                }
            }
        }
    }

    for (cm_in, nf) in cm_to_nf {
        db::upsert_note_spent(
            idx_db,
            &cm_in,
            Some(tx_hash),
            Some(spent_at),
            nf.as_deref(),
            Some(kind),
        )
        .await?;
    }
    Ok(())
}

pub async fn backfill_index(
    da: &DatabaseConnection,
    idx: &DatabaseConnection,
    fvk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<()> {
    let last = db::get_last_processed_id(idx).await?.unwrap_or(0);
    let rows = worker_verified_transactions::Entity::find()
        .filter(worker_verified_transactions::Column::TransactionState.eq(VerifiedState::Accepted))
        .filter(worker_verified_transactions::Column::Id.gt(last))
        .order_by_asc(worker_verified_transactions::Column::Id)
        .limit(500)
        .all(da)
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut cur = last;
    for row in rows.iter() {
        cur = row.id;
        let (kind, amount, anchor_root, nullifiers) = parse_kind_amount_roots(&row.transaction_data)
            .unwrap_or(("other".to_string(), None, None, None));
        let first_nullifier = nullifiers.as_ref().and_then(|v| v.first().cloned());
        let payload = row.transaction_data.clone();
        if kind == "deposit" {
            let sender = row.sender.clone();
            let (rho, recip_from_payload, view_fvks_json) =
                parse_deposit_fields(&row.transaction_data).unwrap_or((None, None, None));
            let view_fvks = row
                .view_fvks_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .or(view_fvks_json);
            let ev_json = extract_events_from_status(row.sequencer_status.as_deref())
                .ok()
                .flatten();
            let deposit_cm_from_events = extract_deposit_commitment(ev_json.as_ref());
            let status = extract_status_from_status(row.sequencer_status.as_deref());
            let event_id = db::insert_event(
                idx,
                &row.tx_hash,
                row.created_at,
                "midnight_privacy",
                &kind,
                &payload,
                status,
                ev_json,
            )
            .await?;
            // Skip if event already exists (duplicate tx_hash)
            let Some(event_id) = event_id else {
                continue;
            };
            // Try to decrypt encrypted notes using the FVK registry
            let encrypted_notes: Option<serde_json::Value> = row
                .encrypted_notes_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            index_output_note_metadata(
                idx,
                &row.tx_hash,
                row.created_at,
                &kind,
                encrypted_notes.as_ref(),
            )
            .await?;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx,
                fvk_registry,
                encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes =
                viewer::try_decrypt_notes_with_registry(fvk_registry, encrypted_notes.as_ref());
            let decrypted_vec = decrypted_notes_from_json(decrypted_notes.as_ref());
            index_output_notes(idx, &row.tx_hash, row.created_at, &kind, &decrypted_vec).await?;

            // Prefer recipient from decrypted notes (already in proper format), fallback to parsed payload
            let recipient = extract_recipient_from_decrypted_notes(decrypted_notes.as_ref())
                .or(recip_from_payload);

            let deposit_cm = deposit_cm_from_events.or_else(|| {
                compute_deposit_commitment_fallback(amount.as_deref(), rho.as_deref(), recipient.as_deref())
            });

            // Deposits may not include any viewer ciphertexts; still index the created output commitment
            // so later spends (cm_ins) can be linked back to this deposit.
            if let Some(cm) = deposit_cm.as_deref() {
                db::upsert_note_created_metadata(idx, cm, &row.tx_hash, row.created_at, &kind)
                    .await?;
            }

            // If there were no encrypted notes (common for deposits), still record deposit note fields
            // using publicly available tx fields + deposit commitment from events.
            let has_encrypted_notes = encrypted_notes
                .as_ref()
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if !has_encrypted_notes {
                if let Some(cm) = deposit_cm.as_deref() {
                    db::upsert_note_created(
                        idx,
                        cm,
                        Some(PRIVACY_DOMAIN_HEX),
                        amount.as_deref(),
                        rho.as_deref(),
                        recipient.as_deref(),
                        recipient.as_deref(),
                        None,
                        Some(&row.tx_hash),
                        Some(row.created_at),
                        Some("deposit"),
                    )
                    .await?;
                }
            }
            db::insert_midnight_deposit(
                idx,
                event_id,
                amount.clone(),
                rho,
                recipient,
                Some(sender.clone()),
                view_fvks,
                encrypted_notes,
            )
            .await?;
        } else if kind == "withdraw" {
            // Prefer recipient stored by worker; fallback to parsing
            let recipient = row.recipient.clone().or_else(|| {
                parse_withdraw_recipient(&row.transaction_data)
                    .ok()
                    .flatten()
            });
            if let Some(recipient) = recipient {
                let ev_json = extract_events_from_status(row.sequencer_status.as_deref())
                    .ok()
                    .flatten();
                let status = extract_status_from_status(row.sequencer_status.as_deref());
                let event_id = db::insert_event(
                    idx,
                    &row.tx_hash,
                    row.created_at,
                    "midnight_privacy",
                    &kind,
                    &payload,
                    status,
                    ev_json,
                )
                .await?;
                // Skip if event already exists (duplicate tx_hash)
                let Some(event_id) = event_id else {
                    continue;
                };
                let view_att: Option<serde_json::Value> = row
                    .view_attestations_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .or_else(|| {
                        parse_withdraw_attestations(&row.proof_outputs)
                            .ok()
                            .flatten()
                            .and_then(|s| serde_json::from_str(&s).ok())
                    });
                // Try to decrypt encrypted notes using the FVK registry
                let encrypted_notes: Option<serde_json::Value> = row
                    .encrypted_notes_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok());
                index_output_note_metadata(
                    idx,
                    &row.tx_hash,
                    row.created_at,
                    &kind,
                    encrypted_notes.as_ref(),
                )
                .await?;
                viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                    idx,
                    fvk_registry,
                    encrypted_notes.as_ref(),
                    fvk_service,
                )
                .await?;
                let decrypted_notes =
                    viewer::try_decrypt_notes_with_registry(fvk_registry, encrypted_notes.as_ref());
                let decrypted_vec = decrypted_notes_from_json(decrypted_notes.as_ref());
                index_output_notes(idx, &row.tx_hash, row.created_at, &kind, &decrypted_vec).await?;
                index_spent_inputs(
                    idx,
                    &row.tx_hash,
                    row.created_at,
                    &kind,
                    nullifiers.as_deref(),
                    &decrypted_vec,
                )
                .await?;
                if let Some(nfs) = nullifiers.as_ref() {
                    for nf in nfs {
                        db::upsert_spent_nullifier(
                            idx,
                            nf,
                            &row.tx_hash,
                            row.created_at,
                            &kind,
                        )
                        .await?;
                    }
                }
                let privacy_sender =
                    extract_sender_from_decrypted_notes(decrypted_notes.as_ref());
                db::insert_midnight_withdraw(
                    idx,
                    event_id,
                    amount.clone(),
                    anchor_root.clone(),
                    first_nullifier.clone(),
                    Some(recipient.clone()),
                    Some(row.sender.clone()),
                    privacy_sender,
                    view_att,
                    encrypted_notes,
                )
                .await?;
            }
        } else if kind == "transfer" {
            let ev_json = extract_events_from_status(row.sequencer_status.as_deref())
                .ok()
                .flatten();
            let status = extract_status_from_status(row.sequencer_status.as_deref());
            let event_id = db::insert_event(
                idx,
                &row.tx_hash,
                row.created_at,
                "midnight_privacy",
                &kind,
                &payload,
                status,
                ev_json,
            )
            .await?;
            // Skip if event already exists (duplicate tx_hash)
            let Some(event_id) = event_id else {
                continue;
            };
            let view_att: Option<serde_json::Value> = row
                .view_attestations_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .or_else(|| {
                    parse_withdraw_attestations(&row.proof_outputs)
                        .ok()
                        .flatten()
                        .and_then(|s| serde_json::from_str(&s).ok())
                });
            // Try to decrypt encrypted notes using the FVK registry
            let encrypted_notes: Option<serde_json::Value> = row
                .encrypted_notes_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            index_output_note_metadata(
                idx,
                &row.tx_hash,
                row.created_at,
                &kind,
                encrypted_notes.as_ref(),
            )
            .await?;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx,
                fvk_registry,
                encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes =
                viewer::try_decrypt_notes_with_registry(fvk_registry, encrypted_notes.as_ref());
            let decrypted_vec = decrypted_notes_from_json(decrypted_notes.as_ref());
            index_output_notes(idx, &row.tx_hash, row.created_at, &kind, &decrypted_vec).await?;
            index_spent_inputs(
                idx,
                &row.tx_hash,
                row.created_at,
                &kind,
                nullifiers.as_deref(),
                &decrypted_vec,
            )
            .await?;
            if let Some(nfs) = nullifiers.as_ref() {
                for nf in nfs {
                    db::upsert_spent_nullifier(idx, nf, &row.tx_hash, row.created_at, &kind)
                        .await?;
                }
            }
            // Extract privacy fields from decrypted notes (as bech32m addresses)
            let recipient = extract_recipient_from_decrypted_notes(decrypted_notes.as_ref());
            let privacy_sender = extract_sender_from_decrypted_notes(decrypted_notes.as_ref());
            let amount = extract_amount_from_decrypted_notes(decrypted_notes.as_ref());
            db::insert_midnight_transfer(
                idx,
                event_id,
                amount,
                anchor_root.clone(),
                first_nullifier.clone(),
                Some(row.sender.clone()),
                privacy_sender,
                recipient,
                view_att,
                encrypted_notes,
                decrypted_notes,
            )
            .await?;
        }
    }
    db::set_last_processed_id(idx, cur).await?;
    Ok(())
}

pub fn spawn_sync_loop(
    da: DatabaseConnection,
    idx: DatabaseConnection,
    fvk_registry: Arc<FvkRegistry>,
    fvk_service: Option<viewer::FvkServiceClient>,
) {
    tokio::spawn(async move {
        use tokio::time::{interval, Duration};
        let mut ticker = interval(Duration::from_millis(1000));
        loop {
            ticker.tick().await;
            if let Err(e) = backfill_index(&da, &idx, &fvk_registry, fvk_service.as_ref()).await {
                tracing::warn!(error = %e, "indexer backfill iteration failed");
            }
        }
    });
}

pub async fn backfill_privacy_fields(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<()> {
    let dep_updates = backfill_deposits(idx_db, vfk_registry, fvk_service).await?;
    let transfer_updates = backfill_transfers(idx_db, vfk_registry, fvk_service).await?;
    let withdraw_updates = backfill_withdraws(idx_db, vfk_registry, fvk_service).await?;

    if dep_updates > 0 || transfer_updates > 0 || withdraw_updates > 0 {
        tracing::info!(
            deposits = dep_updates,
            transfers = transfer_updates,
            withdraws = withdraw_updates,
            "Backfilled privacy fields from encrypted notes"
        );
    }

    Ok(())
}

pub async fn backfill_notes_nullifiers(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<()> {
    let deposits = backfill_notes_nullifiers_deposits(idx_db, vfk_registry, fvk_service).await?;
    let transfers = backfill_notes_nullifiers_transfers(idx_db, vfk_registry, fvk_service).await?;
    let withdraws = backfill_notes_nullifiers_withdraws(idx_db, vfk_registry, fvk_service).await?;

    if deposits > 0 || transfers > 0 || withdraws > 0 {
        tracing::info!(
            deposits,
            transfers,
            withdraws,
            "Backfilled notes_nullifiers from encrypted notes"
        );
    }

    Ok(())
}

/// Backfill the flattened spent-nullifier set from already-indexed `events` rows.
///
/// This is required for correctness when upgrading from single-nullifier transfers to
/// multi-input transfers (up to 4 nullifiers), because the legacy `midnight_transfer.nullifier`
/// column can only store one.
pub async fn backfill_spent_nullifiers(idx_db: &DatabaseConnection) -> Result<usize> {
    const META_KEY: &str = "spent_nullifiers_last_event_id_v1";
    let mut updated = 0usize;
    let mut last_id = db::get_index_meta(idx_db, META_KEY)
        .await?
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);

    loop {
        let rows = idx::Entity::find()
            .filter(idx::Column::Id.gt(last_id))
            .filter(
                Condition::any()
                    .add(idx::Column::Kind.eq("transfer"))
                    .add(idx::Column::Kind.eq("withdraw")),
            )
            .order_by_asc(idx::Column::Id)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        for ev in rows {
            last_id = ev.id;
            let nullifiers = parse_kind_amount_roots(&ev.payload)
                .ok()
                .and_then(|(_, _, _, nfs)| nfs)
                .unwrap_or_default();
            for nf in &nullifiers {
                db::upsert_spent_nullifier(idx_db, nf, &ev.tx_hash, ev.created_at, &ev.kind)
                    .await?;
            }
            db::set_index_meta(idx_db, META_KEY, &last_id.to_string()).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn backfill_notes_nullifiers_deposits(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<usize> {
    // v3: compute deposit `cm` even when `events` are missing/unexpected by falling back to
    // `cm = note_commitment(domain, amount, rho, recipient, sender_id=recipient)`.
    const META_KEY: &str = "notes_nullifiers_deposit_last_event_id_v3";
    let mut updated = 0usize;
    let mut last_id = db::get_index_meta(idx_db, META_KEY)
        .await?
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);

    loop {
        let rows = idx::midnight_deposit::Entity::find()
            .filter(idx::midnight_deposit::Column::EventId.gt(last_id))
            .order_by_asc(idx::midnight_deposit::Column::EventId)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        // Batch-load event metadata for this page.
        let ids: Vec<i32> = rows.iter().map(|r| r.event_id).collect();
        let events = idx::Entity::find()
            .filter(idx::Column::Id.is_in(ids))
            .all(idx_db)
            .await?;
        let mut event_map = std::collections::HashMap::new();
        for ev in events {
            event_map.insert(ev.id, (ev.tx_hash, ev.created_at, ev.events));
        }

        for row in rows {
            last_id = row.event_id;
            let Some((tx_hash, created_at, ev_json)) = event_map.get(&row.event_id) else {
                continue;
            };

            let deposit_cm =
                extract_deposit_commitment(ev_json.as_ref()).or_else(|| {
                    compute_deposit_commitment_fallback(
                        row.amount.as_deref(),
                        row.rho.as_deref(),
                        row.recipient.as_deref(),
                    )
                });
            if let Some(cm) = deposit_cm.as_deref() {
                db::upsert_note_created_metadata(idx_db, cm, tx_hash, *created_at, "deposit")
                    .await?;
            }

            index_output_note_metadata(
                idx_db,
                tx_hash,
                *created_at,
                "deposit",
                row.encrypted_notes.as_ref(),
            )
            .await?;

            let has_encrypted_notes = row
                .encrypted_notes
                .as_ref()
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if !has_encrypted_notes {
                if let Some(cm) = deposit_cm.as_deref() {
                    db::upsert_note_created(
                        idx_db,
                        cm,
                        Some(PRIVACY_DOMAIN_HEX),
                        row.amount.as_deref(),
                        row.rho.as_deref(),
                        row.recipient.as_deref(),
                        row.recipient.as_deref(),
                        None,
                        Some(tx_hash),
                        Some(*created_at),
                        Some("deposit"),
                    )
                    .await?;
                }
            }
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx_db,
                vfk_registry,
                row.encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes =
                viewer::try_decrypt_notes_with_registry(vfk_registry, row.encrypted_notes.as_ref());
            let decrypted_vec = decrypted_notes_from_json(decrypted_notes.as_ref());
            index_output_notes(idx_db, tx_hash, *created_at, "deposit", &decrypted_vec).await?;

            db::set_index_meta(idx_db, META_KEY, &last_id.to_string()).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn backfill_notes_nullifiers_transfers(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<usize> {
    // v2: use tx payload `nullifiers[]` and align with decrypted `cm_ins[]` to fill `spent_nullifier`.
    const META_KEY: &str = "notes_nullifiers_transfer_last_event_id_v2";
    let mut updated = 0usize;
    let mut last_id = db::get_index_meta(idx_db, META_KEY)
        .await?
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);

    loop {
        let rows = idx::midnight_transfer::Entity::find()
            .filter(idx::midnight_transfer::Column::EncryptedNotes.is_not_null())
            .filter(idx::midnight_transfer::Column::EventId.gt(last_id))
            .order_by_asc(idx::midnight_transfer::Column::EventId)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        let ids: Vec<i32> = rows.iter().map(|r| r.event_id).collect();
        let events = idx::Entity::find()
            .filter(idx::Column::Id.is_in(ids))
            .all(idx_db)
            .await?;
        let mut event_map = std::collections::HashMap::new();
        for ev in events {
            event_map.insert(ev.id, (ev.tx_hash, ev.created_at, ev.payload));
        }

        for row in rows {
            last_id = row.event_id;
            let Some((tx_hash, created_at, payload)) = event_map.get(&row.event_id) else {
                continue;
            };
            let nullifiers = parse_kind_amount_roots(payload)
                .ok()
                .and_then(|(_, _, _, nfs)| nfs);

            index_output_note_metadata(
                idx_db,
                tx_hash,
                *created_at,
                "transfer",
                row.encrypted_notes.as_ref(),
            )
            .await?;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx_db,
                vfk_registry,
                row.encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes =
                viewer::try_decrypt_notes_with_registry(vfk_registry, row.encrypted_notes.as_ref());
            let decrypted_vec = decrypted_notes_from_json(decrypted_notes.as_ref());
            index_output_notes(idx_db, tx_hash, *created_at, "transfer", &decrypted_vec).await?;
            index_spent_inputs(
                idx_db,
                tx_hash,
                *created_at,
                "transfer",
                nullifiers.as_deref(),
                &decrypted_vec,
            )
            .await?;
            if let Some(nfs) = nullifiers.as_ref() {
                for nf in nfs {
                    db::upsert_spent_nullifier(idx_db, nf, tx_hash, *created_at, "transfer")
                        .await?;
                }
            }

            db::set_index_meta(idx_db, META_KEY, &last_id.to_string()).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn backfill_notes_nullifiers_withdraws(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<usize> {
    // v2: use tx payload `nullifiers[]` (or `nullifier`) and align with decrypted `cm_ins[]`.
    const META_KEY: &str = "notes_nullifiers_withdraw_last_event_id_v2";
    let mut updated = 0usize;
    let mut last_id = db::get_index_meta(idx_db, META_KEY)
        .await?
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);

    loop {
        let rows = idx::midnight_withdraw::Entity::find()
            .filter(idx::midnight_withdraw::Column::EncryptedNotes.is_not_null())
            .filter(idx::midnight_withdraw::Column::EventId.gt(last_id))
            .order_by_asc(idx::midnight_withdraw::Column::EventId)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        let ids: Vec<i32> = rows.iter().map(|r| r.event_id).collect();
        let events = idx::Entity::find()
            .filter(idx::Column::Id.is_in(ids))
            .all(idx_db)
            .await?;
        let mut event_map = std::collections::HashMap::new();
        for ev in events {
            event_map.insert(ev.id, (ev.tx_hash, ev.created_at, ev.payload));
        }

        for row in rows {
            last_id = row.event_id;
            let Some((tx_hash, created_at, payload)) = event_map.get(&row.event_id) else {
                continue;
            };
            let nullifiers = parse_kind_amount_roots(payload)
                .ok()
                .and_then(|(_, _, _, nfs)| nfs);

            index_output_note_metadata(
                idx_db,
                tx_hash,
                *created_at,
                "withdraw",
                row.encrypted_notes.as_ref(),
            )
            .await?;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx_db,
                vfk_registry,
                row.encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes =
                viewer::try_decrypt_notes_with_registry(vfk_registry, row.encrypted_notes.as_ref());
            let decrypted_vec = decrypted_notes_from_json(decrypted_notes.as_ref());
            index_output_notes(idx_db, tx_hash, *created_at, "withdraw", &decrypted_vec).await?;
            index_spent_inputs(
                idx_db,
                tx_hash,
                *created_at,
                "withdraw",
                nullifiers.as_deref(),
                &decrypted_vec,
            )
            .await?;
            if let Some(nfs) = nullifiers.as_ref() {
                for nf in nfs {
                    db::upsert_spent_nullifier(idx_db, nf, tx_hash, *created_at, "withdraw")
                        .await?;
                }
            }

            db::set_index_meta(idx_db, META_KEY, &last_id.to_string()).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn backfill_deposits(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<usize> {
    let mut updated = 0usize;
    let mut last_id = 0i32;

    loop {
        let rows = idx::midnight_deposit::Entity::find()
            .filter(idx::midnight_deposit::Column::EncryptedNotes.is_not_null())
            .filter(idx::midnight_deposit::Column::Recipient.is_null())
            .filter(idx::midnight_deposit::Column::EventId.gt(last_id))
            .order_by_asc(idx::midnight_deposit::Column::EventId)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        for row in rows {
            last_id = row.event_id;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx_db,
                vfk_registry,
                row.encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes = viewer::try_decrypt_notes_with_registry(
                vfk_registry,
                row.encrypted_notes.as_ref(),
            );
            let Some(decrypted_notes) = decrypted_notes else {
                continue;
            };

            let recipient = if row.recipient.is_none() {
                extract_recipient_from_decrypted_notes(Some(&decrypted_notes))
            } else {
                None
            };

            let mut update = idx::midnight_deposit::ActiveModel {
                event_id: Set(row.event_id),
                ..Default::default()
            };
            if let Some(recipient) = recipient {
                update.recipient = Set(Some(recipient));
            }

            update.update(idx_db).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn backfill_transfers(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<usize> {
    let mut updated = 0usize;
    let mut last_id = 0i32;

    loop {
        let rows = idx::midnight_transfer::Entity::find()
            .filter(idx::midnight_transfer::Column::EncryptedNotes.is_not_null())
            .filter(
                Condition::any()
                    .add(idx::midnight_transfer::Column::Recipient.is_null())
                    .add(idx::midnight_transfer::Column::PrivacySender.is_null())
                    .add(idx::midnight_transfer::Column::DecryptedNotes.is_null())
                    .add(idx::midnight_transfer::Column::Amount.is_null()),
            )
            .filter(idx::midnight_transfer::Column::EventId.gt(last_id))
            .order_by_asc(idx::midnight_transfer::Column::EventId)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        for row in rows {
            last_id = row.event_id;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx_db,
                vfk_registry,
                row.encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes = viewer::try_decrypt_notes_with_registry(
                vfk_registry,
                row.encrypted_notes.as_ref(),
            );
            let Some(decrypted_notes) = decrypted_notes else {
                continue;
            };

            let recipient = if row.recipient.is_none() {
                extract_recipient_from_decrypted_notes(Some(&decrypted_notes))
            } else {
                None
            };
            let privacy_sender = if row.privacy_sender.is_none() {
                extract_sender_from_decrypted_notes(Some(&decrypted_notes))
            } else {
                None
            };
            let amount = if row.amount.is_none() {
                extract_amount_from_decrypted_notes(Some(&decrypted_notes))
            } else {
                None
            };

            let mut update = idx::midnight_transfer::ActiveModel {
                event_id: Set(row.event_id),
                ..Default::default()
            };
            if let Some(recipient) = recipient {
                update.recipient = Set(Some(recipient));
            }
            if let Some(privacy_sender) = privacy_sender {
                update.privacy_sender = Set(Some(privacy_sender));
            }
            if let Some(amount) = amount {
                update.amount = Set(Some(amount));
            }
            if row.decrypted_notes.is_none() {
                update.decrypted_notes = Set(Some(decrypted_notes));
            }

            update.update(idx_db).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

async fn backfill_withdraws(
    idx_db: &DatabaseConnection,
    vfk_registry: &FvkRegistry,
    fvk_service: Option<&viewer::FvkServiceClient>,
) -> Result<usize> {
    let mut updated = 0usize;
    let mut last_id = 0i32;

    loop {
        let rows = idx::midnight_withdraw::Entity::find()
            .filter(idx::midnight_withdraw::Column::EncryptedNotes.is_not_null())
            .filter(idx::midnight_withdraw::Column::PrivacySender.is_null())
            .filter(idx::midnight_withdraw::Column::EventId.gt(last_id))
            .order_by_asc(idx::midnight_withdraw::Column::EventId)
            .limit(500)
            .all(idx_db)
            .await?;

        if rows.is_empty() {
            break;
        }

        for row in rows {
            last_id = row.event_id;
            viewer::maybe_fetch_missing_fvks_for_encrypted_notes(
                idx_db,
                vfk_registry,
                row.encrypted_notes.as_ref(),
                fvk_service,
            )
            .await?;
            let decrypted_notes = viewer::try_decrypt_notes_with_registry(
                vfk_registry,
                row.encrypted_notes.as_ref(),
            );
            let Some(decrypted_notes) = decrypted_notes else {
                continue;
            };

            let privacy_sender = extract_sender_from_decrypted_notes(Some(&decrypted_notes));
            let Some(privacy_sender) = privacy_sender else {
                continue;
            };

            let mut update = idx::midnight_withdraw::ActiveModel {
                event_id: Set(row.event_id),
                ..Default::default()
            };
            update.privacy_sender = Set(Some(privacy_sender));

            update.update(idx_db).await?;
            updated += 1;
        }
    }

    Ok(updated)
}

pub fn parse_kind_amount_roots(
    tx_json: &str,
) -> Result<(String, Option<String>, Option<String>, Option<Vec<String>>)> {
    let v: serde_json::Value = serde_json::from_str(tx_json)?;
    if let Some(obj) = v.get("deposit").and_then(|x| x.as_object()) {
        let amount = obj.get("amount").and_then(|x| match x {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => n.as_u64().map(|u| u.to_string()),
            _ => None,
        });
        return Ok(("deposit".to_string(), amount, None, None));
    }
    if let Some(obj) = v.get("withdraw").and_then(|x| x.as_object()) {
        let amount = obj.get("withdraw_amount").and_then(|x| match x {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => n.as_u64().map(|u| u.to_string()),
            _ => None,
        });
        let anchor_root = obj
            .get("anchor_root")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let nullifiers = obj
            .get("nullifiers")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .or_else(|| {
                obj.get("nullifier")
                    .and_then(|x| x.as_str())
                    .map(|s| vec![s.to_string()])
            });
        return Ok(("withdraw".to_string(), amount, anchor_root, nullifiers));
    }
    if let Some(obj) = v.get("transfer").and_then(|x| x.as_object()) {
        let anchor_root = obj
            .get("anchor_root")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let nullifiers = obj
            .get("nullifiers")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .or_else(|| {
                obj.get("nullifier")
                    .and_then(|x| x.as_str())
                    .map(|s| vec![s.to_string()])
            });
        return Ok(("transfer".to_string(), None, anchor_root, nullifiers));
    }
    Ok(("other".to_string(), None, None, None))
}

pub fn parse_deposit_fields(
    tx_json: &str,
) -> Result<(Option<String>, Option<String>, Option<serde_json::Value>)> {
    let v: serde_json::Value = serde_json::from_str(tx_json)?;
    if let Some(obj) = v.get("deposit").and_then(|x| x.as_object()) {
        let rho = obj
            .get("rho")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        // Handle recipient as either hex string or byte array
        let recip = parse_recipient_to_bech32m(obj.get("recipient"));
        let fvks = obj.get("view_fvks").cloned();
        return Ok((rho, recip, fvks));
    }
    Ok((None, None, None))
}

/// Parse a recipient field (from JSON) and convert to bech32m address.
///
/// Handles multiple formats:
/// - Hex string: "9c66232d..." -> privpool1...
/// - Byte array: [213, 214, 8, ...] -> privpool1...
/// - String array: "[213, 214, 8, ...]" -> privpool1...
/// - Already bech32m: "privpool1..." -> passed through
pub fn parse_recipient_to_bech32m(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;

    // If it's a string
    if let Some(s) = value.as_str() {
        // If it's already a bech32m address, return as-is
        if s.starts_with("privpool1") {
            return Some(s.to_string());
        }

        // Check if it's a string representation of an array like "[47, 239, 50, ...]"
        if s.starts_with('[') && s.ends_with(']') {
            // Try to parse as a JSON array
            if let Ok(arr) = serde_json::from_str::<Vec<u8>>(s) {
                if arr.len() == 32 {
                    let hex_str = hex::encode(&arr);
                    return hex_to_bech32m_address(&hex_str);
                }
            }
        }

        // Otherwise treat as hex and convert to bech32m
        return hex_to_bech32m_address(s);
    }

    // If it's a byte array (JSON array of numbers)
    if let Some(arr) = value.as_array() {
        let bytes: Option<Vec<u8>> = arr.iter().map(|v| v.as_u64().map(|n| n as u8)).collect();
        if let Some(bytes) = bytes {
            if bytes.len() == 32 {
                let hex_str = hex::encode(&bytes);
                return hex_to_bech32m_address(&hex_str);
            }
        }
    }

    None
}

pub fn parse_withdraw_recipient(tx_json: &str) -> Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(tx_json)?;
    if let Some(obj) = v.get("withdraw").and_then(|x| x.as_object()) {
        let to = obj
            .get("to")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        return Ok(to);
    }
    Ok(None)
}

pub fn parse_withdraw_attestations(proof_outputs_json: &str) -> Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(proof_outputs_json)?;
    if let Some(obj) = v.as_object() {
        if let Some(att) = obj.get("view_attestations") {
            return Ok(Some(att.to_string()));
        }
    }
    Ok(None)
}
