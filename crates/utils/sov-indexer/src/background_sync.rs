use crate::db;
use crate::db::{extract_events_from_status, extract_status_from_status};
use crate::index_db as idx;
use crate::viewer::{
    self, extract_recipient_from_decrypted_notes, extract_sender_from_decrypted_notes,
    hex_to_bech32m_address, VfkRegistry,
};
use anyhow::Result;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use sov_midnight_da::storable::worker_verified_transactions;
use sov_midnight_da::storable::worker_verified_transactions::TransactionState as VerifiedState;
use std::sync::Arc;

pub async fn backfill_index(
    da: &DatabaseConnection,
    idx: &DatabaseConnection,
    fvk_registry: &FvkRegistry,
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
        let (kind, amount, anchor_root, nullifier) = parse_kind_amount_roots(&row.transaction_data)
            .unwrap_or(("other".to_string(), None, None, None));
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
            let decrypted_notes = if !fvk_registry.is_empty() {
                viewer::try_decrypt_notes_with_registry(fvk_registry, encrypted_notes.as_ref())
            } else {
                None
            };

            // Prefer recipient from decrypted notes (already in proper format), fallback to parsed payload
            let recipient = extract_recipient_from_decrypted_notes(decrypted_notes.as_ref())
                .or(recip_from_payload);
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
                let decrypted_notes = if !fvk_registry.is_empty() {
                    viewer::try_decrypt_notes_with_registry(fvk_registry, encrypted_notes.as_ref())
                } else {
                    None
                };
                let privacy_sender =
                    extract_sender_from_decrypted_notes(decrypted_notes.as_ref());
                db::insert_midnight_withdraw(
                    idx,
                    event_id,
                    amount.clone(),
                    anchor_root.clone(),
                    nullifier.clone(),
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
            let decrypted_notes = if !fvk_registry.is_empty() {
                viewer::try_decrypt_notes_with_registry(fvk_registry, encrypted_notes.as_ref())
            } else {
                None
            };
            // Extract privacy fields from decrypted notes (as bech32m addresses)
            let recipient = extract_recipient_from_decrypted_notes(decrypted_notes.as_ref());
            let privacy_sender = extract_sender_from_decrypted_notes(decrypted_notes.as_ref());
            db::insert_midnight_transfer(
                idx,
                event_id,
                anchor_root.clone(),
                nullifier.clone(),
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
) {
    tokio::spawn(async move {
        use tokio::time::{interval, Duration};
        let mut ticker = interval(Duration::from_millis(1000));
        loop {
            ticker.tick().await;
            if let Err(e) = backfill_index(&da, &idx, &fvk_registry).await {
                tracing::warn!(error = %e, "indexer backfill iteration failed");
            }
        }
    });
}

pub async fn backfill_privacy_fields(
    idx_db: &DatabaseConnection,
    vfk_registry: &VfkRegistry,
) -> Result<()> {
    if vfk_registry.is_empty() {
        return Ok(());
    }

    let dep_updates = backfill_deposits(idx_db, vfk_registry).await?;
    let transfer_updates = backfill_transfers(idx_db, vfk_registry).await?;
    let withdraw_updates = backfill_withdraws(idx_db, vfk_registry).await?;

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

async fn backfill_deposits(
    idx_db: &DatabaseConnection,
    vfk_registry: &VfkRegistry,
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
    vfk_registry: &VfkRegistry,
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
                    .add(idx::midnight_transfer::Column::DecryptedNotes.is_null()),
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
    vfk_registry: &VfkRegistry,
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
) -> Result<(String, Option<String>, Option<String>, Option<String>)> {
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
        let nullifier = obj
            .get("nullifier")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        return Ok(("withdraw".to_string(), amount, anchor_root, nullifier));
    }
    if let Some(obj) = v.get("transfer").and_then(|x| x.as_object()) {
        let anchor_root = obj
            .get("anchor_root")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let nullifier = obj
            .get("nullifier")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        return Ok(("transfer".to_string(), None, anchor_root, nullifier));
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
