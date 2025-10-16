//! Core wallet operations that are independent of MCP protocol.
//! These functions contain the business logic and are easy to test.

mod get_default_address;
mod get_default_token_balance;
mod update_value_zk;

pub use get_default_address::get_default_address;
pub use get_default_token_balance::get_default_token_balance;
pub use update_value_zk::update_value_zk;
