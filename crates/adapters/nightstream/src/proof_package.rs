//! Proof package for Nightstream proofs.
//!
//! Re-exports the canonical `NightstreamProofPackage` and `Rv32B1RunConfig` from
//! `neo_fold::sovereign_bridge`.  This ensures that the prover (host) and
//! verifier use the exact same serialization format.

pub use neo_fold::sovereign_bridge::{NightstreamProofPackage, Rv32B1RunConfig};
