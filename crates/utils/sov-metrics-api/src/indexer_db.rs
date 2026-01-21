use sea_orm::{entity::prelude::*, JsonValue};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true, column_type = "Integer")]
    pub id: i32,
    #[sea_orm(unique)]
    pub tx_hash: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub module: String,
    pub kind: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub status: Option<String>,
    #[sea_orm(column_type = "Json", nullable)]
    pub events: Option<JsonValue>,
    #[sea_orm(column_type = "Text")]
    pub payload: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub mod midnight_transfer {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "midnight_transfer")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false, column_type = "Integer")]
        pub event_id: i32,
        #[sea_orm(nullable)]
        pub amount: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::Entity",
            from = "Column::EventId",
            to = "super::Column::Id"
        )]
        Events,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod midnight_deposit {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "midnight_deposit")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false, column_type = "Integer")]
        pub event_id: i32,
        #[sea_orm(nullable)]
        pub amount: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::Entity",
            from = "Column::EventId",
            to = "super::Column::Id"
        )]
        Events,
    }

    impl ActiveModelBehavior for ActiveModel {}
}
