//! Core wallet operations that are independent of MCP protocol.
//! These functions contain the business logic and are easy to test.

mod decrypt_transaction;
mod deposit;
mod get_default_address;
mod get_default_token_balance;
mod get_privacy_balance;
mod get_transaction_status;
mod get_transactions;
mod get_wallet_config;
mod transfer;
mod update_value_zk;

pub use decrypt_transaction::decrypt_transaction;
pub use deposit::deposit;
pub use get_default_address::get_default_address;
pub use get_default_token_balance::get_default_token_balance;
pub use get_privacy_balance::get_privacy_balance;
pub use get_transaction_status::get_transaction_status;
pub use get_transactions::get_transactions;
pub use get_wallet_config::get_wallet_config;
pub use transfer::transfer;
pub use update_value_zk::update_value_zk;
