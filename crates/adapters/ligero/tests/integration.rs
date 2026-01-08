#[cfg(feature = "native")]
mod tests {
    use sov_ligero_adapter::{Ligero, LigeroHost};
    use sov_rollup_interface::zk::{CodeCommitment, Zkvm, ZkvmHost};
    fn get_test_program() -> String {
        // Pass a circuit name (or a full `.wasm` path) via LIGERO_PROGRAM_PATH.
        std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "value_validator_rust".to_string())
    }

    #[test]
    fn test_ligero_host_creation() {
        let program = get_test_program();
        let _host = LigeroHost::new(&program);

        // Just verify the host can be created
        assert!(!program.is_empty());
    }

    #[test]
    fn test_code_commitment() {
        let program = get_test_program();
        let host = <Ligero as Zkvm>::Host::from_args(&program);

        let commitment = host.code_commitment();
        let encoded = commitment.encode();

        // Code commitment should be 32 bytes (SHA-256)
        assert_eq!(encoded.len(), 32);

        // Code commitment should be deterministic
        let host2 = <Ligero as Zkvm>::Host::from_args(&program);
        let commitment2 = host2.code_commitment();
        assert_eq!(commitment.encode(), commitment2.encode());
    }

    #[test]
    fn test_code_commitment_decode() {
        let program = get_test_program();
        let host = <Ligero as Zkvm>::Host::from_args(&program);

        let commitment = host.code_commitment();
        let encoded = commitment.encode();

        // Should be able to decode back
        let decoded = sov_ligero_adapter::LigeroCodeCommitment::decode(&encoded).unwrap();
        assert_eq!(commitment.encode(), decoded.encode());
    }

    #[test]
    fn test_host_with_args() {
        let program = get_test_program();
        let mut host = LigeroHost::new(&program);

        // Add some test arguments
        host.add_i64_arg(42);
        host.add_str_arg("test".to_string());
        host.add_hex_arg("abcd1234".to_string());

        // Should not panic
    }

    #[test]
    fn test_host_with_packing() {
        let program = get_test_program();
        let _host = LigeroHost::new(&program).with_packing(4096);

        // Should not panic
    }

    #[test]
    fn test_host_with_private_indices() {
        let program = get_test_program();
        let _host = LigeroHost::new(&program).with_private_indices(vec![1, 2]);

        // Should not panic
    }

    #[test]
    #[ignore] // Only run if webgpu_prover is available
    fn test_proof_generation_if_available() {
        let program = get_test_program();
        let program_path = match ligero_runner::resolve_program(&program) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "Skipping test: failed to resolve program '{}': {}",
                    program, e
                );
                return;
            }
        };

        // Check if the program file exists
        if !program_path.exists() {
            eprintln!(
                "Skipping test: program not found at {}",
                program_path.display()
            );
            return;
        }

        // Check if we can discover a prover binary.
        let paths = match ligero_runner::LigeroPaths::discover() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Skipping test: failed to discover Ligero prover: {}", e);
                return;
            }
        };
        if !paths.prover_bin.exists() {
            eprintln!(
                "Skipping test: webgpu_prover not found at {}",
                paths.prover_bin.display()
            );
            return;
        }

        let mut host = <Ligero as Zkvm>::Host::from_args(&program);
        host.set_public_output(&())
            .expect("bincode serialization must succeed");

        // Try to generate a proof
        // This will fail if WebGPU is not available, but that's ok for CI
        match host.run(true) {
            Ok(proof_data) => {
                println!(
                    "Proof generated successfully, size: {} bytes",
                    proof_data.len()
                );
                assert!(!proof_data.is_empty());
            }
            Err(e) => {
                eprintln!(
                    "Proof generation failed (expected in some environments): {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_simulation_mode() {
        let program = get_test_program();
        let mut host = <Ligero as Zkvm>::Host::from_args(&program);

        // Simulation mode should always work (even without binaries)
        host.set_public_output(&())
            .expect("bincode serialization must succeed");
        let result = host.run(false);

        // Should succeed
        assert!(result.is_ok());
        let proof_data = result.unwrap();

        // Proof data should be a serialized empty package
        assert!(!proof_data.is_empty());
    }
}

/// Test note spending with withdrawal - generates and verifies a REAL ZK proof
///
/// This test demonstrates the full privacy-preserving lifecycle:
/// 1. Create a note commitment using Poseidon2
/// 2. Build Merkle tree and get authentication path
/// 3. Generate a REAL ZK proof using WebGPU
/// 4. Verify the proof and extract public output
#[cfg(feature = "native")]
mod note_spend_tests {
    use anyhow::{Context, Result};
    use ligetron::bn254fr_native::submod_checked;
    use ligetron::poseidon2_hash_bytes as ligetron_hash_bytes;
    use ligetron::Bn254Fr;
    use serde::{Deserialize, Serialize};
    use sov_ligero_adapter::{Ligero, LigeroVerifier};
    use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
    use std::time::Instant;

    type Hash32 = [u8; 32];

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct SpendPublic {
        pub anchor_root: Hash32,
        pub nullifier: Hash32,
        pub withdraw_amount: u128,
        pub output_commitments: Vec<Hash32>,
    }

    fn poseidon2_hash_bytes(data: &[u8]) -> Hash32 {
        let result = ligetron_hash_bytes(data);
        result.to_bytes_be()
    }

    fn poseidon2_hash_domain(tag: &[u8], parts: &[&[u8]]) -> Hash32 {
        let mut tmp = Vec::with_capacity(tag.len() + parts.iter().map(|p| p.len()).sum::<usize>());
        tmp.extend_from_slice(tag);
        for p in parts {
            tmp.extend_from_slice(p);
        }
        poseidon2_hash_bytes(&tmp)
    }

    fn mt_combine(level: u8, left: &Hash32, right: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"MT_NODE_V1", &[&[level], left, right])
    }

    fn note_commitment_v2(
        domain: &Hash32,
        value: u64,
        rho: &Hash32,
        recipient: &Hash32,
        sender_id: &Hash32,
    ) -> Hash32 {
        let mut v16 = [0u8; 16];
        v16[..8].copy_from_slice(&value.to_le_bytes());
        poseidon2_hash_domain(b"NOTE_V2", &[domain, &v16, rho, recipient, sender_id])
    }

    fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"PRF_NF_V1", &[domain, nf_key, rho])
    }

    fn pk_from_sk(spend_sk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"PK_V1", &[spend_sk])
    }

    fn recipient_from_pk(domain: &Hash32, pk_spend: &Hash32, pk_ivk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"ADDR_V2", &[domain, pk_spend, pk_ivk])
    }

    fn recipient_from_sk(domain: &Hash32, spend_sk: &Hash32, pk_ivk: &Hash32) -> Hash32 {
        recipient_from_pk(domain, &pk_from_sk(spend_sk), pk_ivk)
    }

    fn nf_key_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"NFKEY_V1", &[domain, spend_sk])
    }

    struct MerkleTree {
        depth: u8,
        leaves: std::collections::HashMap<usize, Hash32>,
        default_nodes: Vec<Hash32>,
    }

    impl MerkleTree {
        fn new(depth: u8) -> Self {
            let mut default_nodes = vec![[0u8; 32]; depth as usize + 1];
            for level in 1..=depth as usize {
                let prev = default_nodes[level - 1];
                default_nodes[level] = mt_combine((level - 1) as u8, &prev, &prev);
            }
            Self {
                depth,
                leaves: std::collections::HashMap::new(),
                default_nodes,
            }
        }

        fn set_leaf(&mut self, pos: usize, leaf: Hash32) {
            self.leaves.insert(pos, leaf);
        }

        fn get_leaf(&self, pos: usize) -> Hash32 {
            *self.leaves.get(&pos).unwrap_or(&self.default_nodes[0])
        }

        fn root(&self) -> Hash32 {
            self.compute_node(0, self.depth)
        }

        fn compute_node(&self, pos: usize, level: u8) -> Hash32 {
            if level == 0 {
                return self.get_leaf(pos);
            }
            let left = self.compute_node(pos * 2, level - 1);
            let right = self.compute_node(pos * 2 + 1, level - 1);
            let default = self.default_nodes[(level - 1) as usize];
            if left == default && right == default {
                return self.default_nodes[level as usize];
            }
            mt_combine(level - 1, &left, &right)
        }

        fn open(&self, pos: usize) -> Vec<Hash32> {
            let mut siblings = Vec::with_capacity(self.depth as usize);
            let mut idx = pos;
            for level in 0..self.depth {
                siblings.push(self.compute_node(idx ^ 1, level));
                idx /= 2;
            }
            siblings
        }
    }

    fn get_note_spend_program_path() -> Result<String> {
        // Pass a circuit name (or a full `.wasm` path) via LIGERO_PROGRAM_PATH.
        Ok(std::env::var("LIGERO_PROGRAM_PATH").unwrap_or_else(|_| "note_spend_guest".to_string()))
    }

    fn prover_available() -> bool {
        // Use ligero-runner's discovery mechanism to find the prover binary
        match ligero_runner::LigeroPaths::discover() {
            Ok(paths) => paths.prover_bin.exists(),
            Err(_) => false,
        }
    }

    fn hex32(h: &Hash32) -> String {
        format!("0x{}", hex::encode(h))
    }

    fn bn254fr_from_hash32_be(h: &Hash32) -> Bn254Fr {
        let mut out = Bn254Fr::new();
        out.set_bytes_big(h);
        out
    }

    /// Test spending with withdrawal (mixed shielded + transparent)
    ///
    /// Spends a 500-unit note, withdraws 200 units transparently, and creates
    /// a 300-unit shielded change note. Generates and verifies a REAL ZK proof.
    #[test]
    fn test_note_spend_with_withdrawal() -> Result<()> {
        println!("\n=== Note Spend with Withdrawal Test ===\n");

        let program_path = match get_note_spend_program_path() {
            Ok(p) => p,
            Err(e) => {
                println!("⚠️ Skipping test: {}", e);
                return Ok(());
            }
        };

        if !prover_available() {
            println!("⚠️ Skipping test: WebGPU prover not available");
            return Ok(());
        }

        // Create input note
        let domain: Hash32 = [1u8; 32];
        let value: u64 = 500;
        let rho: Hash32 = [2u8; 32];
        let spend_sk: Hash32 = [4u8; 32];
        let pk_ivk_owner: Hash32 = [6u8; 32];

        let recipient_owner = recipient_from_sk(&domain, &spend_sk, &pk_ivk_owner);
        let nf_key = nf_key_from_sk(&domain, &spend_sk);
        let sender_id_in: Hash32 = [3u8; 32];
        let cm = note_commitment_v2(&domain, value, &rho, &recipient_owner, &sender_id_in);

        println!("Input note: {} units", value);

        // Build tree
        let tree_depth: u8 = 16;
        let mut tree = MerkleTree::new(tree_depth);
        let position: u64 = 0;
        tree.set_leaf(position as usize, cm);
        let anchor = tree.root();
        let siblings = tree.open(position as usize);
        let nf = nullifier(&domain, &nf_key, &rho);

        // Withdraw some, keep rest as shielded change
        let withdraw_amount: u64 = 200;
        let withdraw_to: Hash32 = [9u8; 32];
        let change_value: u64 = 300;
        let change_rho: Hash32 = [10u8; 32];
        let change_pk_spend: Hash32 = [11u8; 32];
        let change_pk_ivk: Hash32 = [12u8; 32];
        let change_rcp = recipient_from_pk(&domain, &change_pk_spend, &change_pk_ivk);
        let sender_id_out = recipient_owner;
        let cm_change = note_commitment_v2(
            &domain,
            change_value,
            &change_rho,
            &change_rcp,
            &sender_id_out,
        );

        println!("Withdraw: {} units (transparent)", withdraw_amount);
        println!("Change: {} units (shielded)", change_value);
        assert_eq!(value, withdraw_amount + change_value, "Balance check");

        let public_output = SpendPublic {
            anchor_root: anchor,
            nullifier: nf,
            withdraw_amount: withdraw_amount.into(),
            output_commitments: vec![cm_change],
        };

        // === ARGUMENT LAYOUT (matches note_spend_guest v2) ===
        //
        // Header:
        //   1: domain [PUBLIC]
        //   2: spend_sk [PRIVATE]
        //   3: pk_ivk_owner [PRIVATE]
        //   4: depth [PUBLIC]
        //   5: anchor [PUBLIC]
        //   6: n_in [PUBLIC]
        //
        // Per-input (n_in=1 here):
        //   value_in, rho_in, sender_id_in, pos_bits[depth], siblings[depth], nullifier
        //
        // Then:
        //   withdraw_amount [PUBLIC]
        //   withdraw_to [PUBLIC]
        //   n_out [PUBLIC]
        //
        // Per-output:
        //   value_out, rho_out, pk_spend_out, pk_ivk_out, cm_out
        //
        // Finally:
        //   inv_enforce [PRIVATE] (field inverse witness)

        let depth = tree_depth as usize;
        let n_in: usize = 1;
        let n_out: usize = 1;

        // Compute inv_enforce witness to satisfy the circuit's single-inverse enforcement:
        // enforce_prod = (v_in * v_out) * (rho_out - rho_in)
        // inv_enforce = enforce_prod^{-1}
        let v_in_fr = Bn254Fr::from_u64(value);
        let v_out_fr = Bn254Fr::from_u64(change_value);
        let rho_in_fr = bn254fr_from_hash32_be(&rho);
        let rho_out_fr = bn254fr_from_hash32_be(&change_rho);

        let mut enforce_prod = Bn254Fr::from_u32(1);
        enforce_prod.mulmod_checked(&v_in_fr);
        enforce_prod.mulmod_checked(&v_out_fr);
        let mut delta = Bn254Fr::new();
        submod_checked(&mut delta, &rho_out_fr, &rho_in_fr);
        enforce_prod.mulmod_checked(&delta);

        let mut inv_enforce_fr = enforce_prod.clone();
        inv_enforce_fr.inverse();
        let inv_enforce = inv_enforce_fr.to_bytes_be();

        // Configure private indices (1-based indexing).
        let mut private_indices: Vec<usize> = Vec::new();
        // Header privates.
        private_indices.extend_from_slice(&[2, 3]);
        // Input privates.
        let mut idx: usize = 7;
        for _ in 0..n_in {
            private_indices.extend_from_slice(&[idx, idx + 1, idx + 2]); // value, rho, sender_id
            idx += 3;
            for _ in 0..depth {
                private_indices.push(idx); // pos_bit
                idx += 1;
            }
            for _ in 0..depth {
                private_indices.push(idx); // sibling
                idx += 1;
            }
            idx += 1; // nullifier (public)
        }
        idx += 3; // withdraw_amount, withdraw_to, n_out (public)
                  // Output privates.
        for _ in 0..n_out {
            private_indices.extend_from_slice(&[idx, idx + 1, idx + 2, idx + 3]); // v, rho, pk_spend, pk_ivk
            idx += 5; // skip cm_out (public)
        }
        // inv_enforce (private).
        private_indices.push(idx);

        println!("✓ Private indices: {:?}", private_indices);

        let mut host =
            <Ligero as Zkvm>::Host::from_args(&program_path).with_private_indices(private_indices);

        // Header.
        host.add_hex_arg(hex32(&domain));
        host.add_hex_arg(hex32(&spend_sk));
        host.add_hex_arg(hex32(&pk_ivk_owner));
        host.add_u64_arg(tree_depth as u64);
        host.add_hex_arg(hex32(&anchor));
        host.add_u64_arg(n_in as u64);

        // Input 0.
        host.add_u64_arg(value);
        host.add_hex_arg(hex32(&rho));
        host.add_hex_arg(hex32(&sender_id_in));

        // Position bits (LSB-first), each passed as 32-byte BE 0 or 1.
        for level in 0..depth {
            let bit = ((position >> level) & 1) as u8;
            let mut bit_bytes = [0u8; 32];
            bit_bytes[31] = bit;
            host.add_hex_arg(hex32(&bit_bytes));
        }

        // Siblings (bottom-up).
        for sibling in &siblings {
            host.add_hex_arg(hex32(sibling));
        }

        // Public nullifier.
        host.add_hex_arg(hex32(&nf));

        // Withdraw binding.
        host.add_u64_arg(withdraw_amount);
        host.add_hex_arg(hex32(&withdraw_to));
        host.add_u64_arg(n_out as u64);

        // Output 0.
        host.add_u64_arg(change_value);
        host.add_hex_arg(hex32(&change_rho));
        host.add_hex_arg(hex32(&change_pk_spend));
        host.add_hex_arg(hex32(&change_pk_ivk));
        host.add_hex_arg(hex32(&cm_change));

        // inv_enforce (private).
        host.add_hex_arg(hex32(&inv_enforce));

        host.set_public_output(&public_output)?;

        let code_commitment = host.code_commitment();

        println!("\n==================== PROVER ====================\n");

        let proof_start = Instant::now();
        let (proof_data, prover_stdout) = host
            .run_with_logging()
            .context("Failed to generate proof")?;
        let proof_time = proof_start.elapsed();

        // Print full prover output
        println!("{}", prover_stdout);

        // Extract stats from prover output
        let linear = prover_stdout
            .lines()
            .find(|l| l.contains("Num Linear constraints:"))
            .and_then(|l| l.split_whitespace().last())
            .unwrap_or("?");
        let quadratic = prover_stdout
            .lines()
            .find(|l| l.contains("Num quadratic constraints:"))
            .and_then(|l| l.split_whitespace().last())
            .unwrap_or("?");

        println!(
            "\n✓ Proof generated: {} bytes ({:.3}s)",
            proof_data.len(),
            proof_time.as_secs_f64()
        );

        println!("\n==================== VERIFIER ====================\n");

        // Set environment variables for verifier
        // SAFETY: This is a single-threaded test, no concurrent access to env vars
        unsafe {
            std::env::set_var("LIGERO_PROGRAM_PATH", host.program_path());
            std::env::set_var("LIGERO_SHADER_PATH", host.shader_path());
            std::env::set_var("LIGERO_PACKING", host.packing().to_string());
            std::env::set_var(
                "LIGERO_VERIFIER_BIN",
                host.verifier_bin().to_string_lossy().to_string(),
            );
        }

        let verify_start = Instant::now();
        let verify_result: Result<(SpendPublic, String, String)> =
            LigeroVerifier::verify_with_output(&proof_data, &code_commitment);
        let verify_time = verify_start.elapsed();

        let verified = match verify_result {
            Ok((public, verifier_stdout, verifier_stderr)) => {
                // Print full verifier output
                println!("{}", verifier_stdout);
                if !verifier_stderr.is_empty() {
                    eprintln!("Verifier stderr:\n{}", verifier_stderr);
                }
                println!("\n✓ Proof verified ({:.3}s)", verify_time.as_secs_f64());
                public
            }
            Err(e) => {
                // Still try to get the verifier output from the error message
                eprintln!(
                    "\n✗ Verification FAILED ({:.3}s)",
                    verify_time.as_secs_f64()
                );
                eprintln!("Error: {:#}", e);
                return Err(e);
            }
        };

        println!("\n==============================================");
        println!("                 SUMMARY                      ");
        println!("==============================================");
        println!();
        println!("=== CONSTRAINTS ===");
        println!("Linear Constraints:    {}", linear);
        println!("Quadratic Constraints: {}", quadratic);
        println!();
        println!("=== TIMING ===");
        println!("  Prover Time:   {:.0}ms", proof_time.as_millis());
        println!("  Verifier Time: {:.0}ms", verify_time.as_millis());
        println!("  ─────────────────────────────");
        println!(
            "  Total Time:    {:.0}ms",
            (proof_time + verify_time).as_millis()
        );
        println!("==============================================");

        assert_eq!(verified.withdraw_amount, withdraw_amount as u128);
        assert_eq!(verified.output_commitments.len(), 1);
        assert_eq!(verified.output_commitments[0], cm_change);

        println!("\n=== Test Complete ===");
        println!(
            "✓ Withdrawal: {} transparent + {} shielded change",
            withdraw_amount, change_value
        );

        Ok(())
    }
}
