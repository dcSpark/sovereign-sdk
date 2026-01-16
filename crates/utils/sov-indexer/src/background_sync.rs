use crate::db;
use crate::db::{extract_events_from_status, extract_status_from_status};
use crate::viewer::{
    self, extract_recipient_from_decrypted_notes, hex_to_bech32m_address, FvkRegistry,
};
use anyhow::Result;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
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
                decrypted_notes,
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
                db::insert_midnight_withdraw(
                    idx,
                    event_id,
                    amount.clone(),
                    anchor_root.clone(),
                    nullifier.clone(),
                    Some(recipient.clone()),
                    Some(row.sender.clone()),
                    view_att,
                    encrypted_notes,
                    decrypted_notes,
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
            // Extract recipient from decrypted notes (as bech32m address)
            let recipient = extract_recipient_from_decrypted_notes(decrypted_notes.as_ref());
            db::insert_midnight_transfer(
                idx,
                event_id,
                anchor_root.clone(),
                nullifier.clone(),
                Some(row.sender.clone()),
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
/// - Already bech32m: "privpool1..." -> passed through
pub fn parse_recipient_to_bech32m(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;

    // If it's a string
    if let Some(s) = value.as_str() {
        // If it's already a bech32m address, return as-is
        if s.starts_with("privpool1") {
            return Some(s.to_string());
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
