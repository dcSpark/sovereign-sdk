//! Ligetron Rust SDK
//! 
//! This crate provides Rust bindings for the Ligetron zero-knowledge proof system.
//! 
//! ## Modules
//! 
//! - [`api`] - Core API functions
//! - [`sha2`] - SHA-256 hash function
//! - [`bn254fr`] - BN254 scalar field arithmetic
//! - [`vbn254fr`] - Vectorized BN254 operations
//! - [`poseidon`] - Poseidon hash function (t=3, t=5)
//! - [`poseidon2`] - Poseidon2 hash function (t=2)
//! - [`babyjubjub`] - Baby Jubjub elliptic curve operations
//! - [`eddsa`] - Edwards-curve Digital Signature Algorithm
//! ```

// Native implementations (for testing outside WASM)
#[cfg(feature = "native")]
pub mod bn254fr_native;
#[cfg(feature = "native")]
pub mod poseidon2_native;

// WASM implementations (using host functions)
#[cfg(not(feature = "native"))]
pub mod api;
#[cfg(not(feature = "native"))]
pub mod sha2;
#[cfg(not(feature = "native"))]
pub mod bn254fr;
#[cfg(not(feature = "native"))]
pub mod vbn254fr;
#[cfg(not(feature = "native"))]
pub mod poseidon;
#[cfg(not(feature = "native"))]
pub mod poseidon2;
#[cfg(not(feature = "native"))]
pub mod babyjubjub;
#[cfg(not(feature = "native"))]
pub mod eddsa;

// Shared modules
mod poseidon2_constant;
#[cfg(not(feature = "native"))]
mod poseidon_constant;

// Re-export for convenience
#[cfg(not(feature = "native"))]
pub use api::*;

#[cfg(feature = "native")]
pub use bn254fr_native::{Bn254Fr, addmod_checked, mulmod_checked};
#[cfg(feature = "native")]
pub use poseidon2_native::{Poseidon2Context, poseidon2_hash, poseidon2_hash_bytes};