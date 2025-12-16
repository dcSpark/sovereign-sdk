//! Native BN254 Scalar Field Operations using ark-bn254
//! 
//! This module provides the same API as bn254fr.rs but uses ark-bn254
//! for actual field arithmetic instead of host function calls.
//! Used for testing outside the WASM environment.

use ark_bn254::Fr;
use ark_ff::{BigInteger, PrimeField, Field, Zero};
use num_bigint::BigUint;
use std::ffi::CStr;
use std::str::FromStr;

/// A BN254 scalar field element (native implementation)
#[derive(Clone, Debug)]
pub struct Bn254Fr {
    value: Fr,
}

impl Bn254Fr {
    /// Create a new zero field element
    #[inline(always)]
    pub fn new() -> Self {
        Self { value: Fr::from(0u64) }
    }

    /// Construct from u32 constant
    pub fn from_u32(value: u32) -> Self {
        Self { value: Fr::from(value as u64) }
    }

    /// Construct from u64 constant
    pub fn from_u64(value: u64) -> Self {
        Self { value: Fr::from(value) }
    }

    /// Construct field element from a C-style string (decimal or hex with 0x prefix)
    pub fn from_c_str(str_ptr: *const i8) -> Self {
        let c_str = unsafe { CStr::from_ptr(str_ptr) };
        let s = c_str.to_str().expect("Invalid UTF-8");
        Self::from_str(s)
    }

    /// Construct field element from a string (decimal or hex with 0x prefix)
    pub fn from_str(s: &str) -> Self {
        let s = s.trim();
        let value = if s.starts_with("0x") || s.starts_with("0X") {
            // Parse as hex
            let hex_str = &s[2..];
            let bytes = hex::decode(hex_str).expect("Invalid hex string");
            // Pad to 32 bytes
            let mut padded = [0u8; 32];
            let start = 32 - bytes.len();
            padded[start..].copy_from_slice(&bytes);
            Fr::from_be_bytes_mod_order(&padded)
        } else {
            // Parse as decimal
            let bigint = BigUint::from_str(s).expect("Invalid decimal string");
            let bytes = bigint.to_bytes_be();
            let mut padded = [0u8; 32];
            let start = 32usize.saturating_sub(bytes.len());
            let len = bytes.len().min(32);
            padded[start..].copy_from_slice(&bytes[bytes.len() - len..]);
            Fr::from_be_bytes_mod_order(&padded)
        };
        Self { value }
    }

    /// Set field element to a u32 constant
    pub fn set_u32(&mut self, value: u32) {
        self.value = Fr::from(value as u64);
    }

    /// Set field element to a u64 constant
    pub fn set_u64(&mut self, value: u64) {
        self.value = Fr::from(value);
    }

    /// Set field element from a string
    pub fn set_str(&mut self, s: &str, _base: u32) {
        *self = Self::from_str(s);
    }

    /// Set field element from byte buffer in big-endian order
    pub fn set_bytes_big(&mut self, bytes: &[u8]) {
        let mut padded = [0u8; 32];
        let len = bytes.len().min(32);
        let start = 32 - len;
        padded[start..].copy_from_slice(&bytes[..len]);
        self.value = Fr::from_be_bytes_mod_order(&padded);
    }

    /// Set field element from byte buffer in little-endian order
    pub fn set_bytes_little(&mut self, bytes: &[u8]) {
        let mut padded = [0u8; 32];
        let len = bytes.len().min(32);
        padded[..len].copy_from_slice(&bytes[..len]);
        self.value = Fr::from_le_bytes_mod_order(&padded);
    }

    /// Get field element as bytes in big-endian order
    pub fn get_bytes_big(&self, out: &mut [u8]) {
        let bytes = self.value.into_bigint().to_bytes_be();
        let out_len = out.len();
        out.fill(0);
        let copy_len = bytes.len().min(out_len);
        let out_start = out_len.saturating_sub(bytes.len());
        let bytes_start = bytes.len().saturating_sub(out_len);
        out[out_start..].copy_from_slice(&bytes[bytes_start..bytes_start + copy_len]);
    }

    /// Get field element as bytes in little-endian order
    pub fn get_bytes_little(&self, out: &mut [u8]) {
        let bytes = self.value.into_bigint().to_bytes_le();
        out.fill(0);
        let len = bytes.len().min(out.len());
        out[..len].copy_from_slice(&bytes[..len]);
    }

    /// Get field element as 32 bytes in big-endian order
    pub fn to_bytes_be(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        self.get_bytes_big(&mut out);
        out
    }

    /// Print field element (hex)
    pub fn print_hex(&self) {
        let bytes = self.to_bytes_be();
        println!("0x{}", hex::encode(bytes));
    }

    /// Print field element (decimal)
    pub fn print_dec(&self) {
        println!("{}", self.value);
    }

    /// Assert two field elements are equal
    pub fn assert_equal(a: &Self, b: &Self) {
        assert_eq!(a.value, b.value, "Field elements not equal");
    }

    /// Add modular (checked - creates constraint in zkVM, just adds in native)
    pub fn addmod_checked(&mut self, other: &Self) {
        self.value += other.value;
    }

    /// Multiply modular (checked)
    pub fn mulmod_checked(&mut self, other: &Self) {
        self.value *= other.value;
    }

    /// Subtract modular (checked)
    pub fn submod_checked(&mut self, other: &Self) {
        self.value -= other.value;
    }

    /// Negate
    pub fn neg(&mut self) {
        self.value = -self.value;
    }

    /// Square
    pub fn square(&mut self) {
        self.value = self.value.square();
    }

    /// Inverse
    pub fn inverse(&mut self) {
        self.value = self.value.inverse().expect("Cannot invert zero");
    }

    /// Check if zero
    pub fn is_zero(&self) -> bool {
        self.value.is_zero()
    }

    /// Get internal value (for native operations)
    pub fn inner(&self) -> &Fr {
        &self.value
    }
}

/// Add two field elements (checked)
pub fn addmod_checked(out: &mut Bn254Fr, a: &Bn254Fr, b: &Bn254Fr) {
    out.value = a.value + b.value;
}

/// Multiply two field elements (checked)
pub fn mulmod_checked(out: &mut Bn254Fr, a: &Bn254Fr, b: &Bn254Fr) {
    out.value = a.value * b.value;
}

/// Subtract two field elements (checked)
pub fn submod_checked(out: &mut Bn254Fr, a: &Bn254Fr, b: &Bn254Fr) {
    out.value = a.value - b.value;
}

