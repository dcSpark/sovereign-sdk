use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true, column_type = "Integer")]
    pub id: i32,
    pub tx_hash: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub module: String,
    pub kind: String,
    #[sea_orm(column_type = "Text")]
    pub payload: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::index_db::involvement::Entity")]
    Involvement,
}

impl ActiveModelBehavior for ActiveModel {}

pub mod involvement {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "involvement")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true, column_type = "Integer")]
        pub id: i32,
        pub event_id: i32,
        pub address: String,
        pub role: String,
        pub direction: String,
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

    impl Related<super::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Events.def()
        }
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
        pub amount: Option<String>,
        pub rho: Option<String>,
        pub recipient: Option<String>,
        #[sea_orm(nullable)]
        pub sender: Option<String>,
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

pub mod midnight_withdraw {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "midnight_withdraw")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false, column_type = "Integer")]
        pub event_id: i32,
        pub amount: Option<String>,
        pub anchor_root: Option<String>,
        pub nullifier: Option<String>,
        #[sea_orm(column_name = "to", nullable)]
        pub to_addr: Option<String>,
        #[sea_orm(nullable)]
        pub sender: Option<String>,
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

pub mod index_meta {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "index_meta")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub key: String,
        pub value: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
