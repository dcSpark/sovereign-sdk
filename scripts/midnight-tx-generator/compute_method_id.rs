#!/usr/bin/env rust-script
//! Compute the method ID (code commitment) for the note spend circuit
//! TODO: Migrate to Nightstream - was using Ligero Host::code_commitment()
//!
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! hex = "0.4"
//! sov-rollup-interface = { path = "../../crates/rollup-interface" }
//! ```

use anyhow::Result;

fn main() -> Result<()> {
    // TODO: Migrate to Nightstream - was using Ligero Host::from_args + code_commitment
    todo!("TODO: Migrate to Nightstream - Ligero method ID computation")
}

