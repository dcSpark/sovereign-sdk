use anyhow::{Context, Result};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait,
    FromQueryResult, QueryFilter, QuerySelect, Statement,
};

use crate::indexer_db::{midnight_deposit, midnight_transfer};
use crate::materialized_views::{INDEXER_DEPOSIT_TOTALS_VIEW, INDEXER_TRANSFER_TOTALS_VIEW};

#[derive(Debug, FromQueryResult)]
struct TransferAmountAggregateRow {
    total_amount: String,
    total_transactions: i64,
}

#[derive(Debug, FromQueryResult)]
struct TotalAmountAggregateRow {
    total_amount: String,
}

pub(crate) async fn transfer_amount_totals(db: &DatabaseConnection) -> Result<(u128, u64)> {
    if db.get_database_backend() == DatabaseBackend::Postgres {
        let row = transfer_amount_totals_from_mv(db)
            .await?
            .context("Missing row in transfer totals materialized view")?;
        return parse_transfer_aggregate(row);
    }

    transfer_amount_totals_fallback(db).await
}

pub(crate) async fn deposit_total_amount(db: &DatabaseConnection) -> Result<u128> {
    if db.get_database_backend() == DatabaseBackend::Postgres {
        let row = deposit_total_amount_from_mv(db)
            .await?
            .context("Missing row in deposit totals materialized view")?;
        return parse_total_amount(row.total_amount, "deposit");
    }

    deposit_total_amount_fallback(db).await
}

async fn transfer_amount_totals_from_mv(
    db: &DatabaseConnection,
) -> Result<Option<TransferAmountAggregateRow>> {
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "
            SELECT total_amount, total_transactions
            FROM {INDEXER_TRANSFER_TOTALS_VIEW}
            WHERE id = 1
            "
        ),
    );

    TransferAmountAggregateRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query transfer totals materialized view")
}

async fn deposit_total_amount_from_mv(
    db: &DatabaseConnection,
) -> Result<Option<TotalAmountAggregateRow>> {
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "
            SELECT total_amount
            FROM {INDEXER_DEPOSIT_TOTALS_VIEW}
            WHERE id = 1
            "
        ),
    );

    TotalAmountAggregateRow::find_by_statement(stmt)
        .one(db)
        .await
        .context("Failed to query deposit totals materialized view")
}

fn parse_transfer_aggregate(row: TransferAmountAggregateRow) -> Result<(u128, u64)> {
    let total_amount = parse_total_amount(row.total_amount, "transfer")?;
    let total_transactions = u64::try_from(row.total_transactions)
        .with_context(|| format!("Negative transfer count: {}", row.total_transactions))?;
    Ok((total_amount, total_transactions))
}

fn parse_total_amount(total_amount: String, kind: &str) -> Result<u128> {
    total_amount
        .parse::<u128>()
        .with_context(|| format!("Invalid aggregated {kind} amount: {total_amount}"))
}

async fn transfer_amount_totals_fallback(db: &DatabaseConnection) -> Result<(u128, u64)> {
    let amounts = midnight_transfer::Entity::find()
        .select_only()
        .column(midnight_transfer::Column::Amount)
        .filter(midnight_transfer::Column::Amount.is_not_null())
        .into_tuple::<Option<String>>()
        .all(db)
        .await
        .context("Failed to load transfer amounts")?;

    let mut total_amount: u128 = 0;
    let mut total_transactions: u64 = 0;
    for amount in amounts {
        let amount = amount
            .as_ref()
            .context("Missing transfer amount")?
            .parse::<u128>()
            .context("Invalid transfer amount")?;
        total_amount = total_amount
            .checked_add(amount)
            .context("Transfer amount overflow")?;
        total_transactions += 1;
    }

    Ok((total_amount, total_transactions))
}

async fn deposit_total_amount_fallback(db: &DatabaseConnection) -> Result<u128> {
    let amounts = midnight_deposit::Entity::find()
        .select_only()
        .column(midnight_deposit::Column::Amount)
        .filter(midnight_deposit::Column::Amount.is_not_null())
        .into_tuple::<Option<String>>()
        .all(db)
        .await
        .context("Failed to load deposit amounts")?;

    let mut total_amount: u128 = 0;
    for amount in amounts {
        let amount = amount
            .as_ref()
            .context("Missing deposit amount")?
            .parse::<u128>()
            .context("Invalid deposit amount")?;
        total_amount = total_amount
            .checked_add(amount)
            .context("Deposit amount overflow")?;
    }

    Ok(total_amount)
}
