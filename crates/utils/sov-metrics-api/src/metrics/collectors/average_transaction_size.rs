//! Intentionally schema-only module.
//!
//! The historical `AverageTransactionSizeCollector` was removed on purpose to avoid
//! duplicate periodic DB reads. The API still serves average transaction size by:
//! - reading the dedicated materialized view on Postgres, or
//! - deriving it from `token-value-spent` counter samples in tsink.
//!
//! We keep the payload type here because API decoding and OpenAPI schemas still rely on it.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct AverageTransactionSizePayload {
    pub total_amount: String,
    pub total_transactions: u64,
}
