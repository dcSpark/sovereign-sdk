//! Native Poseidon2 Hash Function using ark-bn254
//!
//! This module provides the same API as poseidon2.rs but uses native
//! field arithmetic via bn254fr_native.

use crate::bn254fr_native::{Bn254Fr, addmod_checked, mulmod_checked};
use crate::poseidon2_constant::{
    POSEIDON2_T2_RC_STR,
    POSEIDON2_BN254_RP,
};

/// Poseidon2 hash context for BN254 field elements (t=2)
pub struct Poseidon2Context {
    state: [Bn254Fr; 2],
    buffer: Vec<u8>,
    buffer_len: usize,
    temp: Bn254Fr,
    rc: Vec<Bn254Fr>,
}

impl Poseidon2Context {
    pub fn new() -> Self {
        let rc = POSEIDON2_T2_RC_STR.iter()
            .map(|&s| Bn254Fr::from_str(s))
            .collect();

        Poseidon2Context {
            state: [
                Bn254Fr::from_u32(0),
                Bn254Fr::from_u32(0),
            ],
            buffer: vec![0u8; 31],
            buffer_len: 0,
            temp: Bn254Fr::new(),
            rc,
        }
    }

    /// Reset the internal context state
    pub fn digest_init(&mut self) {
        self.state[0] = Bn254Fr::from_u32(0);
        self.state[1] = Bn254Fr::from_u32(0);
        self.buffer_len = 0;
        for i in 0..31 {
            self.buffer[i] = 0;
        }
    }

    pub fn digest_update(&mut self, data: &Bn254Fr) {
        self.state[0].addmod_checked(data);
        self.permute();
    }

    pub fn digest_update_bytes(&mut self, data: &[u8]) {
        let mut offset = 0;
        let mut remaining = data.len();

        // Process 31-byte chunks
        while remaining >= 31 {
            let chunk = &data[offset..offset + 31];
            self.temp.set_bytes_big(chunk);
            self.state[0].addmod_checked(&self.temp);
            self.permute();
            offset += 31;
            remaining -= 31;
        }

        // Handle remaining bytes
        for &byte in &data[offset..] {
            self.buffer[self.buffer_len] = byte;
            self.buffer_len += 1;

            if self.buffer_len >= 31 {
                self.temp.set_bytes_big(&self.buffer[..31]);
                self.state[0].addmod_checked(&self.temp);
                self.permute();
                self.buffer_len = 0;
            }
        }
    }

    /// Finalize the hash computation and get the result
    pub fn digest_final(&mut self) -> Bn254Fr {
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;

        while self.buffer_len < 31 {
            self.buffer[self.buffer_len] = 0;
            self.buffer_len += 1;
        }

        self.temp.set_bytes_big(&self.buffer[..31]);
        self.state[0].addmod_checked(&self.temp);
        self.permute();

        self.state[0].clone()
    }

    /// Internal permutation function for Poseidon2
    fn permute(&mut self) {
        // External MDS multiplication
        self.multiply_external_mds();

        let mut round = 0;

        // First half of full rounds (4 rounds)
        for _ in 0..4 {
            self.add_round_constants(round);
            self.sbox_full();
            self.multiply_external_mds();
            round += 1;
        }

        // Partial rounds (56 rounds)
        for _ in 0..POSEIDON2_BN254_RP {
            self.add_round_constants_partial(round);
            self.sbox_partial();
            self.multiply_internal_mds();
            round += 1;
        }

        // Second half of full rounds (4 rounds)
        for _ in 0..4 {
            self.add_round_constants(round);
            self.sbox_full();
            self.multiply_external_mds();
            round += 1;
        }
    }

    /// Add round constants to the state (full rounds)
    fn add_round_constants(&mut self, round: usize) {
        self.state[0].addmod_checked(&self.rc[round * 2]);
        self.state[1].addmod_checked(&self.rc[round * 2 + 1]);
    }

    /// Add round constants to the state (partial rounds - only first element)
    fn add_round_constants_partial(&mut self, round: usize) {
        self.state[0].addmod_checked(&self.rc[round * 2]);
    }

    /// Apply S-box (x^5) to all elements
    fn sbox_full(&mut self) {
        self.state[0] = self.pow5(&self.state[0]);
        self.state[1] = self.pow5(&self.state[1]);
    }

    /// Apply S-box (x^5) to first element only
    fn sbox_partial(&mut self) {
        self.state[0] = self.pow5(&self.state[0]);
    }

    /// Compute x^5 for field element
    fn pow5(&self, x: &Bn254Fr) -> Bn254Fr {
        let mut x2 = Bn254Fr::new();
        let mut result = Bn254Fr::new();
        mulmod_checked(&mut x2, x, x);          // x^2
        mulmod_checked(&mut result, &x2, &x2);  // x^4
        result.mulmod_checked(x);               // x^5 = x^4 * x
        result
    }

    /// External MDS matrix multiplication for t=2
    /// External MDS = [2, 1]
    ///                [1, 2]
    fn multiply_external_mds(&mut self) {
        addmod_checked(&mut self.temp, &self.state[0], &self.state[1]);
        self.state[0].addmod_checked(&self.temp);
        self.state[1].addmod_checked(&self.temp);
    }

    /// Internal MDS matrix multiplication for t=2
    /// Internal MDS = [2, 1]
    ///                [1, 3]
    fn multiply_internal_mds(&mut self) {
        addmod_checked(&mut self.temp, &self.state[0], &self.state[1]);
        self.state[0].addmod_checked(&self.temp);
        self.temp.addmod_checked(&self.state[1]);
        self.state[1].addmod_checked(&self.temp);
    }
}

/// Convenience function to compute Poseidon2 hash from field elements
pub fn poseidon2_hash(inputs: &[Bn254Fr]) -> Bn254Fr {
    let mut ctx = Poseidon2Context::new();
    for input in inputs {
        ctx.digest_update(input);
    }
    ctx.digest_final()
}

/// Convenience function to compute Poseidon2 hash from bytes
pub fn poseidon2_hash_bytes(data: &[u8]) -> Bn254Fr {
    let mut ctx = Poseidon2Context::new();
    ctx.digest_update_bytes(data);
    ctx.digest_final()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poseidon2_matches_ligetron() {
        // Known test vector from Ligetron prover:
        // hash(0xFF * 32) = 0x20d0a48cbc645db12d09c441d41e6d5975d2f115f32b2e01d4a9bc6d87070bbc
        let input = [0xFFu8; 32];
        let result = poseidon2_hash_bytes(&input);
        
        let mut result_bytes = [0u8; 32];
        result.get_bytes_big(&mut result_bytes);
        let result_hex = hex::encode(&result_bytes);
        
        let expected = "20d0a48cbc645db12d09c441d41e6d5975d2f115f32b2e01d4a9bc6d87070bbc";
        
        println!("Input:    {} (32 bytes of 0xFF)", hex::encode(&input));
        println!("Expected: {}", expected);
        println!("Got:      {}", result_hex);
        
        assert_eq!(result_hex.to_lowercase(), expected.to_lowercase(), 
            "Hash mismatch! Native implementation doesn't match Ligetron.");
    }
}

