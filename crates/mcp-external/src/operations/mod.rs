//! Core wallet operations that are independent of MCP protocol.
//! These functions contain the business logic and are easy to test.

/// Default max fee applied to MCP-generated transactions.
pub const DEFAULT_MAX_FEE: u128 = 2_000_000u128;

pub mod decrypt_transaction;
mod deposit;
mod get_default_address;
mod get_privacy_balance;
mod get_transaction_status;
mod get_transactions;
mod get_wallet_config;
mod send_funds;
mod transfer;
mod pool_admin;
pub mod wallet_status;

#[allow(unused_imports)]
pub use decrypt_transaction::decrypt_transaction;
pub use deposit::deposit;
pub use get_default_address::get_default_address;
pub use get_privacy_balance::{get_privacy_balance, PrivacyBalanceResult, UnspentNote};
pub use get_transaction_status::get_transaction_status;
pub use get_transactions::get_transactions;
pub use get_transactions::Transaction;
#[allow(unused_imports)]
pub use get_wallet_config::get_wallet_config;
pub use transfer::transfer;
pub use get_transaction_status::TransactionDetails;
pub use pool_admin::{
    add_pool_admin, freeze_address, list_frozen_addresses, remove_pool_admin, unfreeze_address,
};
pub use send_funds::send_funds;
