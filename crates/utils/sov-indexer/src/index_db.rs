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
        #[sea_orm(nullable, column_type = "Json")]
        pub view_fvks: Option<JsonValue>,
        #[sea_orm(nullable, column_type = "Json")]
        pub encrypted_notes: Option<JsonValue>,
        // Decrypted note fields (populated when AUTHORITY_VFK is configured)
        #[sea_orm(nullable, column_type = "Json")]
        pub decrypted_notes: Option<JsonValue>,
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
        #[sea_orm(nullable, column_type = "Json")]
        pub view_attestations: Option<JsonValue>,
        #[sea_orm(nullable, column_type = "Json")]
        pub encrypted_notes: Option<JsonValue>,
        // Decrypted note fields (populated when AUTHORITY_VFK is configured)
        #[sea_orm(nullable, column_type = "Json")]
        pub decrypted_notes: Option<JsonValue>,
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

pub mod midnight_transfer {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "midnight_transfer")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false, column_type = "Integer")]
        pub event_id: i32,
        pub anchor_root: Option<String>,
        pub nullifier: Option<String>,
        #[sea_orm(nullable)]
        pub sender: Option<String>,
        /// Recipient address (bech32m format, e.g., privpool1...)
        #[sea_orm(nullable)]
        pub recipient: Option<String>,
        #[sea_orm(nullable, column_type = "Json")]
        pub view_attestations: Option<JsonValue>,
        #[sea_orm(nullable, column_type = "Json")]
        pub encrypted_notes: Option<JsonValue>,
        // Decrypted note fields (populated when AUTHORITY_VFK is configured)
        #[sea_orm(nullable, column_type = "Json")]
        pub decrypted_notes: Option<JsonValue>,
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

/// Registry of known VFKs for decryption.
/// Maps fvk_commitment -> (vfk, shielded_address) for looking up which key to use.
pub mod vfk_registry {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "vfk_registry")]
    pub struct Model {
        /// FVK commitment: H("FVK_COMMIT_V1" || vfk) - primary key for lookups
        #[sea_orm(primary_key, auto_increment = false, column_type = "String(StringLen::N(64))")]
        pub fvk_commitment: String,
        /// The actual VFK (32 bytes hex-encoded)
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub vfk: String,
        /// The shielded address associated with this VFK (bech32 or hex, optional)
        #[sea_orm(column_type = "Text", nullable)]
        pub shielded_address: Option<String>,
        /// When this entry was added
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
