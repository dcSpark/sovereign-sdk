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
        /// Privacy sender (bech32m), derived from decrypted notes when available
        #[sea_orm(nullable)]
        pub privacy_sender: Option<String>,
        #[sea_orm(nullable, column_type = "Json")]
        pub view_attestations: Option<JsonValue>,
        #[sea_orm(nullable, column_type = "Json")]
        pub encrypted_notes: Option<JsonValue>,
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
        /// Amount transferred (from decrypted notes)
        #[sea_orm(nullable)]
        pub amount: Option<String>,
        pub anchor_root: Option<String>,
        pub nullifier: Option<String>,
        #[sea_orm(nullable)]
        pub sender: Option<String>,
        /// Privacy sender (bech32m), derived from decrypted notes when available
        #[sea_orm(nullable)]
        pub privacy_sender: Option<String>,
        /// Recipient address (bech32m format, e.g., privpool1...)
        #[sea_orm(nullable)]
        pub recipient: Option<String>,
        #[sea_orm(nullable, column_type = "Json")]
        pub view_attestations: Option<JsonValue>,
        #[sea_orm(nullable, column_type = "Json")]
        pub encrypted_notes: Option<JsonValue>,
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
    impl Related<super::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Events.def()
        }
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// Flattened set of spent nullifiers (one row per nullifier).
///
/// This supports multi-input transfers (up to 4 nullifiers) without requiring schema changes
/// to `midnight_transfer.nullifier`.
pub mod midnight_spent_nullifiers {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "midnight_spent_nullifiers")]
    pub struct Model {
        /// Nullifier (32 bytes hex, no 0x prefix)
        #[sea_orm(
            primary_key,
            auto_increment = false,
            column_type = "String(StringLen::N(64))"
        )]
        pub nullifier: String,

        /// Tx hash that spent this nullifier (e.g. 0x...)
        #[sea_orm(column_type = "Text")]
        pub spent_tx_hash: String,

        /// Timestamp of the spending tx (copied from `events.created_at`)
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub spent_at: chrono::DateTime<chrono::Utc>,

        /// Kind of spending tx: transfer / withdraw
        #[sea_orm(column_type = "Text")]
        pub spent_kind: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Registry of known FVKs for decryption.
/// Maps fvk_commitment -> (fvk, shielded_address, wallet_address) for looking up which key to use.
pub mod fvk_registry {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "fvk_registry")]
    pub struct Model {
        /// FVK commitment: H("FVK_COMMIT_V1" || fvk) - primary key for lookups
        #[sea_orm(
            primary_key,
            auto_increment = false,
            column_type = "String(StringLen::N(64))"
        )]
        pub fvk_commitment: String,
        /// The actual FVK (32 bytes hex-encoded)
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub fvk: String,
        /// The shielded address associated with this FVK (bech32 privpool1..., optional)
        #[sea_orm(column_type = "Text", nullable)]
        pub shielded_address: Option<String>,
        /// The public wallet address associated with this FVK (sov1..., optional)
        #[sea_orm(column_type = "Text", nullable)]
        pub wallet_address: Option<String>,
        /// When this entry was added
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Tracks prefunded wallets that can be claimed by external services (e.g. MCP).
///
/// This table intentionally stores only non-secret metadata (addresses + claim state).
pub mod prefunded_wallets {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "prefunded_wallets")]
    pub struct Model {
        /// Public wallet address (sov1...) - primary key for lookups
        #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
        pub wallet_address: String,

        /// Privacy address (bech32m privpool1...) associated with this wallet
        #[sea_orm(column_type = "Text")]
        pub privacy_address: String,

        /// Whether this wallet has been claimed/assigned
        pub used: bool,

        /// When this entry was added
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub created_at: chrono::DateTime<chrono::Utc>,

        /// When this wallet was claimed (if used)
        #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
        pub claimed_at: Option<chrono::DateTime<chrono::Utc>>,

        /// Optional identifier for who claimed this wallet (e.g. MCP session id)
        #[sea_orm(column_type = "Text", nullable)]
        pub claimed_by: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Tracks frozen accounts with their freeze/unfreeze history and reasons.
pub mod frozen_accounts {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "frozen_accounts")]
    pub struct Model {
        /// Unique ID for this freeze event
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i64,
        /// Privacy address (bech32m privpool1...)
        #[sea_orm(column_type = "Text")]
        pub privacy_address: String,
        /// Public wallet address (sov1...) if known
        #[sea_orm(column_type = "Text", nullable)]
        pub wallet_address: Option<String>,
        /// Reason for freeze/unfreeze action
        #[sea_orm(column_type = "Text", nullable)]
        pub reason: Option<String>,
        /// Whether this is a freeze (true) or unfreeze (false) event
        pub is_frozen: bool,
        /// Transaction hash that performed this action
        #[sea_orm(column_type = "String(StringLen::N(64))", nullable)]
        pub tx_hash: Option<String>,
        /// Who initiated this action (admin address)
        #[sea_orm(column_type = "Text", nullable)]
        pub initiated_by: Option<String>,
        /// When this action occurred
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// UTXO-like note tracking table for auditors/indexers.
///
/// One row per note commitment (`cm`). When a note is spent, `spent_*` fields are set
/// by observing a later tx that reveals the input commitments (e.g. via `cm_ins` in
/// decrypted output plaintexts).
pub mod notes_nullifiers {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "notes_nullifiers")]
    pub struct Model {
        /// Note commitment (32 bytes hex, no 0x prefix)
        #[sea_orm(
            primary_key,
            auto_increment = false,
            column_type = "String(StringLen::N(64))"
        )]
        pub cm: String,

        /// Decrypted domain (32 bytes hex)
        #[sea_orm(column_type = "String(StringLen::N(64))", nullable)]
        pub domain: Option<String>,

        /// Decrypted value (u128 as decimal string)
        #[sea_orm(nullable)]
        pub value: Option<String>,

        /// Decrypted rho (32 bytes hex)
        #[sea_orm(column_type = "String(StringLen::N(64))", nullable)]
        pub rho: Option<String>,

        /// Decrypted recipient as bech32m (e.g. `privpool1...`)
        #[sea_orm(column_type = "Text", nullable)]
        pub recipient: Option<String>,

        /// Decrypted sender_id as bech32m (e.g. `privpool1...`), when present
        #[sea_orm(column_type = "Text", nullable)]
        pub sender_id: Option<String>,

        /// Commitments of notes spent to create this tx (padded), as JSON array of hex strings.
        #[sea_orm(column_type = "Json", nullable)]
        pub cm_ins: Option<JsonValue>,

        /// Tx hash that created this note (e.g. 0x...)
        #[sea_orm(column_type = "Text", nullable)]
        pub created_tx_hash: Option<String>,

        /// Timestamp of the creating tx (copied from `events.created_at`)
        #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
        pub created_at: Option<chrono::DateTime<chrono::Utc>>,

        /// Kind of creating tx: deposit / transfer / withdraw
        #[sea_orm(column_type = "Text", nullable)]
        pub created_kind: Option<String>,

        /// Tx hash that spent this note (e.g. 0x...)
        #[sea_orm(column_type = "Text", nullable)]
        pub spent_tx_hash: Option<String>,

        /// Timestamp of the spending tx (copied from `events.created_at`)
        #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
        pub spent_at: Option<chrono::DateTime<chrono::Utc>>,

        /// Public nullifier that spent this note (32 bytes hex, with/without 0x)
        #[sea_orm(column_type = "String(StringLen::N(64))", nullable, unique)]
        pub spent_nullifier: Option<String>,

        /// Kind of spending tx: transfer / withdraw
        #[sea_orm(column_type = "Text", nullable)]
        pub spent_kind: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
