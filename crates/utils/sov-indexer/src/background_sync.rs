use anyhow::Result;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, DatabaseConnection, QuerySelect};
use sov_midnight_da::storable::worker_verified_transactions;
use sov_midnight_da::storable::worker_verified_transactions::TransactionState as VerifiedState;
use crate::db;

pub async fn backfill_index(da: &DatabaseConnection, idx: &DatabaseConnection) -> Result<()> {
    let last = db::get_last_processed_id(idx).await?.unwrap_or(0);
    let rows = worker_verified_transactions::Entity::find()
        .filter(worker_verified_transactions::Column::TransactionState.eq(VerifiedState::Accepted))
        .filter(worker_verified_transactions::Column::Id.gt(last))
        .order_by_asc(worker_verified_transactions::Column::Id)
        .limit(500)
        .all(da)
        .await?;
    if rows.is_empty() { return Ok(()); }
    let mut cur = last;
    for row in rows.iter() {
        cur = row.id;
        let (kind, amount, anchor_root, nullifier) = parse_kind_amount_roots(&row.transaction_data).unwrap_or(("other".to_string(), None, None, None));
        let payload = row.transaction_data.clone();
        if kind == "deposit" {
            let sender = row.sender.clone();
            let (rho, recip_hex) = parse_deposit_fields(&row.transaction_data).unwrap_or((None, None));
            let event_id = db::insert_event(idx, &row.tx_hash, row.created_at, "midnight_privacy", &kind, &payload).await?;
            db::insert_midnight_deposit(idx, event_id, amount.clone(), rho, recip_hex, Some(sender.clone())).await?;
            db::insert_involvement(idx, event_id, &sender, "sender", "out").await?;
        } else if kind == "withdraw" {
            if let Some(recipient) = parse_withdraw_recipient(&row.transaction_data).ok().flatten() {
                let event_id = db::insert_event(idx, &row.tx_hash, row.created_at, "midnight_privacy", &kind, &payload).await?;
                db::insert_midnight_withdraw(idx, event_id, amount.clone(), anchor_root.clone(), nullifier.clone(), Some(recipient.clone()), Some(row.sender.clone())).await?;
                db::insert_involvement(idx, event_id, &recipient, "recipient", "in").await?;
                db::insert_involvement(idx, event_id, &row.sender, "sender", "out").await?;
            }
        }
    }
    db::set_last_processed_id(idx, cur).await?;
    Ok(())
}

pub fn spawn_sync_loop(da: DatabaseConnection, idx: DatabaseConnection) {
    tokio::spawn(async move {
        use tokio::time::{interval, Duration};
        let mut ticker = interval(Duration::from_millis(1000));
        loop {
            ticker.tick().await;
            if let Err(e) = backfill_index(&da, &idx).await {
                tracing::warn!(error = %e, "indexer backfill iteration failed");
            }
        }
    });
}

fn parse_kind_amount_roots(tx_json: &str) -> Result<(String, Option<String>, Option<String>, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(tx_json)?;
    if let Some(obj) = v.get("deposit").and_then(|x| x.as_object()) {
        let amount = obj.get("amount").and_then(|x| match x { serde_json::Value::String(s) => Some(s.clone()), serde_json::Value::Number(n) => n.as_u64().map(|u| u.to_string()), _ => None });
        return Ok(("deposit".to_string(), amount, None, None));
    }
    if let Some(obj) = v.get("withdraw").and_then(|x| x.as_object()) {
        let amount = obj.get("withdraw_amount").and_then(|x| match x { serde_json::Value::String(s) => Some(s.clone()), serde_json::Value::Number(n) => n.as_u64().map(|u| u.to_string()), _ => None });
        let anchor_root = obj.get("anchor_root").and_then(|x| x.as_str()).map(|s| s.to_string());
        let nullifier = obj.get("nullifier").and_then(|x| x.as_str()).map(|s| s.to_string());
        return Ok(("withdraw".to_string(), amount, anchor_root, nullifier));
    }
    Ok(("other".to_string(), None, None, None))
}

fn parse_deposit_fields(tx_json: &str) -> Result<(Option<String>, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(tx_json)?;
    if let Some(obj) = v.get("deposit").and_then(|x| x.as_object()) {
        let rho = obj.get("rho").and_then(|x| x.as_str()).map(|s| s.to_string());
        let recip = obj.get("recipient").and_then(|x| x.as_str()).map(|s| s.to_string());
        return Ok((rho, recip));
    }
    Ok((None, None))
}

fn parse_withdraw_recipient(tx_json: &str) -> Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(tx_json)?;
    if let Some(obj) = v.get("withdraw").and_then(|x| x.as_object()) {
        let to = obj.get("to").and_then(|x| x.as_str()).map(|s| s.to_string());
        return Ok(to);
    }
    Ok(None)
}
