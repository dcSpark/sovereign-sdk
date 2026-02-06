use anyhow::{Context, Result};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait,
    FromQueryResult, QueryFilter, QuerySelect, Statement,
};

use crate::indexer_db::{midnight_deposit, midnight_transfer};

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
        let stmt = Statement::from_string(
            DatabaseBackend::Postgres,
            r#"
            SELECT
                COALESCE(SUM(CAST(amount AS NUMERIC)), 0)::text AS total_amount,
                COUNT(*)::bigint AS total_transactions
            FROM midnight_transfer
            WHERE amount IS NOT NULL
            "#
            .to_owned(),
        );

        let row = TransferAmountAggregateRow::find_by_statement(stmt)
            .one(db)
            .await
            .context("Failed to aggregate midnight_transfer totals")?
            .context("Missing aggregate row for midnight_transfer totals")?;

        return parse_transfer_aggregate(row);
    }

    transfer_amount_totals_fallback(db).await
}

pub(crate) async fn deposit_total_amount(db: &DatabaseConnection) -> Result<u128> {
    if db.get_database_backend() == DatabaseBackend::Postgres {
        let stmt = Statement::from_string(
            DatabaseBackend::Postgres,
            r#"
            SELECT
                COALESCE(SUM(CAST(amount AS NUMERIC)), 0)::text AS total_amount
            FROM midnight_deposit
            WHERE amount IS NOT NULL
            "#
            .to_owned(),
        );

        let row = TotalAmountAggregateRow::find_by_statement(stmt)
            .one(db)
            .await
            .context("Failed to aggregate midnight_deposit totals")?
            .context("Missing aggregate row for midnight_deposit totals")?;

        return parse_total_amount(row.total_amount, "deposit");
    }

    deposit_total_amount_fallback(db).await
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
