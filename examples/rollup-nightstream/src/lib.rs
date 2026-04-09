//! Nightstream rollup using Sovereign SDK with Midnight DA and MockZkvm.
#![deny(missing_docs)]

use std::str::FromStr;

mod midnight_bridge;
mod nightstream_rollup;
pub use nightstream_rollup::*;

// TODO: https://github.com/Sovereign-Labs/sovereign-sdk-wip/issues/387
fn eth_dev_signer() -> sov_ethereum::Signers {
    sov_ethereum::Signers::new(vec![secp256k1::SecretKey::from_str(
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    )
    .unwrap()])
}
