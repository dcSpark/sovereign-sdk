use anyhow::Result;
use sea_orm::{entity::prelude::*, sea_query::{Index, IndexCreateStatement, OnConflict}, Schema, DatabaseConnection, Set};
use chrono::{DateTime, Utc};
use crate::index_db as idx;

pub async fn init_index_db(idx_db: &DatabaseConnection) -> Result<()> {
    let builder = idx_db.get_database_backend();
    let schema = Schema::new(builder);
    let stmt = builder.build(&schema.create_table_from_entity(idx::Entity).if_not_exists().to_owned());
    idx_db.execute(stmt).await?;
    let stmt = builder.build(&schema.create_table_from_entity(idx::involvement::Entity).if_not_exists().to_owned());
    idx_db.execute(stmt).await?;
    let stmt = builder.build(&schema.create_table_from_entity(idx::midnight_deposit::Entity).if_not_exists().to_owned());
    idx_db.execute(stmt).await?;
    let stmt = builder.build(&schema.create_table_from_entity(idx::midnight_withdraw::Entity).if_not_exists().to_owned());
    idx_db.execute(stmt).await?;
    let stmt = builder.build(&schema.create_table_from_entity(idx::index_meta::Entity).if_not_exists().to_owned());
    idx_db.execute(stmt).await?;
    let inv_idx: IndexCreateStatement = Index::create()
        .name("idx_involvement_address")
        .table(idx::involvement::Entity)
        .col(idx::involvement::Column::Address)
        .if_not_exists()
        .to_owned();
    idx_db.execute(builder.build(&inv_idx)).await?;
    Ok(())
}

pub async fn get_last_processed_id(idx_db: &DatabaseConnection) -> Result<Option<i32>> {
    let row = idx::index_meta::Entity::find_by_id("last_id").one(idx_db).await?;
    Ok(row.map(|m| m.value.parse().unwrap_or(0)))
}

pub async fn set_last_processed_id(idx_db: &DatabaseConnection, id: i32) -> Result<()> {
    let _ = idx::index_meta::Entity::insert(idx::index_meta::ActiveModel { key: Set("last_id".to_string()), value: Set(id.to_string()) })
        .on_conflict(OnConflict::column(idx::index_meta::Column::Key).do_nothing().to_owned())
        .exec(idx_db)
        .await?;
    let _ = idx::index_meta::Entity::update(idx::index_meta::ActiveModel { key: Set("last_id".to_string()), value: Set(id.to_string()) }).exec(idx_db).await;
    Ok(())
}

pub async fn insert_event(idx_db: &DatabaseConnection, tx_hash: &str, created_at: DateTime<Utc>, module: &str, kind: &str, payload: &str) -> Result<i32> {
    let res = idx::Entity::insert(idx::ActiveModel {
        tx_hash: Set(tx_hash.to_string()),
        created_at: Set(created_at),
        module: Set(module.to_string()),
        kind: Set(kind.to_string()),
        payload: Set(payload.to_string()),
        ..Default::default()
    }).exec(idx_db).await?;
    Ok(res.last_insert_id)
}

pub async fn insert_involvement(idx_db: &DatabaseConnection, event_id: i32, address: &str, role: &str, direction: &str) -> Result<()> {
    let _ = idx::involvement::Entity::insert(idx::involvement::ActiveModel {
        event_id: Set(event_id),
        address: Set(address.to_string()),
        role: Set(role.to_string()),
        direction: Set(direction.to_string()),
        ..Default::default()
    }).exec(idx_db).await?;
    Ok(())
}

pub async fn insert_midnight_deposit(idx_db: &DatabaseConnection, event_id: i32, amount: Option<String>, rho: Option<String>, recipient: Option<String>, sender: Option<String>) -> Result<()> {
    let _ = idx::midnight_deposit::Entity::insert(idx::midnight_deposit::ActiveModel {
        event_id: Set(event_id), amount: Set(amount), rho: Set(rho), recipient: Set(recipient), sender: Set(sender)
    }).exec(idx_db).await?;
    Ok(())
}

pub async fn insert_midnight_withdraw(idx_db: &DatabaseConnection, event_id: i32, amount: Option<String>, anchor_root: Option<String>, nullifier: Option<String>, to: Option<String>, sender: Option<String>) -> Result<()> {
    let _ = idx::midnight_withdraw::Entity::insert(idx::midnight_withdraw::ActiveModel {
        event_id: Set(event_id), amount: Set(amount), anchor_root: Set(anchor_root), nullifier: Set(nullifier), to_addr: Set(to), sender: Set(sender)
    }).exec(idx_db).await?;
    Ok(())
}
