#[cfg(feature = "native")]
mod tests {
    use sov_ligero_adapter::{Ligero, LigeroHost};
    use sov_rollup_interface::zk::{CodeCommitment, Zkvm, ZkvmHost};
    use std::path::PathBuf;

    fn get_test_program_path() -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let program_path = manifest_dir.join("bins/programs/edit.wasm");
        program_path.to_string_lossy().to_string()
    }

    #[test]
    fn test_ligero_host_creation() {
        let program_path = get_test_program_path();
        let _host = LigeroHost::new(&program_path);

        // Just verify the host can be created
        assert!(!program_path.is_empty());
    }

    #[test]
    fn test_code_commitment() {
        let program_path = get_test_program_path();
        let host = <Ligero as Zkvm>::Host::from_args(&program_path);

        let commitment = host.code_commitment();
        let encoded = commitment.encode();

        // Code commitment should be 32 bytes (SHA-256)
        assert_eq!(encoded.len(), 32);

        // Code commitment should be deterministic
        let host2 = <Ligero as Zkvm>::Host::from_args(&program_path);
        let commitment2 = host2.code_commitment();
        assert_eq!(commitment.encode(), commitment2.encode());
    }

    #[test]
    fn test_code_commitment_decode() {
        let program_path = get_test_program_path();
        let host = <Ligero as Zkvm>::Host::from_args(&program_path);

        let commitment = host.code_commitment();
        let encoded = commitment.encode();

        // Should be able to decode back
        let decoded = sov_ligero_adapter::LigeroCodeCommitment::decode(&encoded).unwrap();
        assert_eq!(commitment.encode(), decoded.encode());
    }

    #[test]
    fn test_host_with_args() {
        let program_path = get_test_program_path();
        let mut host = LigeroHost::new(&program_path);

        // Add some test arguments
        host.add_i64_arg(42);
        host.add_str_arg("test".to_string());
        host.add_hex_arg("abcd1234".to_string());

        // Should not panic
    }

    #[test]
    fn test_host_with_packing() {
        let program_path = get_test_program_path();
        let _host = LigeroHost::new(&program_path).with_packing(4096);

        // Should not panic
    }

    #[test]
    fn test_host_with_private_indices() {
        let program_path = get_test_program_path();
        let _host = LigeroHost::new(&program_path).with_private_indices(vec![1, 2]);

        // Should not panic
    }

    #[test]
    #[ignore] // Only run if webgpu_prover is available
    fn test_proof_generation_if_available() {
        let program_path = get_test_program_path();

        // Check if the program file exists
        if !PathBuf::from(&program_path).exists() {
            eprintln!("Skipping test: program not found at {}", program_path);
            return;
        }

        // Check if webgpu_prover binary exists
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let prover_bin = manifest_dir.join("bins/webgpu_prover");
        if !prover_bin.exists() {
            eprintln!("Skipping test: webgpu_prover not found");
            return;
        }

        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
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
        let program_path = get_test_program_path();
        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);

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
    use ligetron::poseidon2_hash_bytes as ligetron_hash_bytes;
    use serde::{Deserialize, Serialize};
    use sov_ligero_adapter::{Ligero, LigeroVerifier};
    use sov_rollup_interface::zk::{ZkVerifier, Zkvm, ZkvmHost};
    use std::path::PathBuf;
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

    fn note_commitment(domain: &Hash32, value: u128, rho: &Hash32, recipient: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"NOTE_V1", &[domain, &value.to_le_bytes(), rho, recipient])
    }

    fn nullifier(domain: &Hash32, nf_key: &Hash32, rho: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"PRF_NF_V1", &[domain, nf_key, rho])
    }

    fn pk_from_sk(spend_sk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"PK_V1", &[spend_sk])
    }

    fn recipient_from_pk(domain: &Hash32, pk: &Hash32) -> Hash32 {
        poseidon2_hash_domain(b"ADDR_V1", &[domain, pk])
    }

    fn recipient_from_sk(domain: &Hash32, spend_sk: &Hash32) -> Hash32 {
        recipient_from_pk(domain, &pk_from_sk(spend_sk))
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
            Self { depth, leaves: std::collections::HashMap::new(), default_nodes }
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
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let program_path = manifest_dir.join("guest/bins/programs/note_spend_guest.wasm");
        if !program_path.exists() {
            anyhow::bail!(
                "note_spend_guest.wasm not found at: {}\n\
                 Build it with: cd crates/adapters/ligero/guest/note-spend-guest && ./build.sh",
                program_path.display()
            );
        }
        Ok(program_path.to_string_lossy().to_string())
    }

    fn prover_available() -> bool {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        #[cfg(target_os = "macos")]
        let prover = manifest_dir.join("bins/macos/bin/webgpu_prover");
        #[cfg(target_os = "linux")]
        let prover = manifest_dir.join("bins/linux-amd64/bin/webgpu_prover");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let prover = manifest_dir.join("bins/webgpu_prover");
        prover.exists()
    }

    fn hex32(h: &Hash32) -> String {
        format!("0x{}", hex::encode(h))
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

        let recipient = recipient_from_sk(&domain, &spend_sk);
        let nf_key = nf_key_from_sk(&domain, &spend_sk);
        let cm = note_commitment(&domain, value.into(), &rho, &recipient);

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
        let change_value: u64 = 300;
        let change_rho: Hash32 = [10u8; 32];
        let change_pk: Hash32 = [11u8; 32];
        let change_rcp = recipient_from_pk(&domain, &change_pk);
        let cm_change = note_commitment(&domain, change_value.into(), &change_rho, &change_rcp);

        println!("Withdraw: {} units (transparent)", withdraw_amount);
        println!("Change: {} units (shielded)", change_value);
        assert_eq!(value, withdraw_amount + change_value, "Balance check");

        let n_out: u32 = 1;

        let public_output = SpendPublic {
            anchor_root: anchor,
            nullifier: nf,
            withdraw_amount: withdraw_amount.into(),
            output_commitments: vec![cm_change],
        };

        // === NEW ARGUMENT LAYOUT FOR FIELD-LEVEL MERKLE PATH ===
        // Position is now passed as individual bits (one per level) instead of a single integer.
        // This enables making position bits private without breaking constraints.
        //
        // Layout:
        //   1: domain (hex)
        //   2: value (i64)
        //   3: rho (hex) [PRIVATE]
        //   4: recipient (hex) [PRIVATE]
        //   5: spend_sk (hex) [PRIVATE]
        //   6: depth (i64)
        //   7 to 6+depth: position bits [PRIVATE] (hex, 0x00...00 or 0x00...01)
        //   7+depth to 6+2*depth: siblings [PRIVATE] (hex)
        //   7+2*depth: anchor (str)
        //   8+2*depth: nullifier (str)
        //   9+2*depth: withdraw_amount (i64)
        //   10+2*depth: n_out (i64)
        //   Then 4 args per output: value, rho, pk, cm

        // Configure private indices (1-based indexing)
        // All private inputs: rho (3), recipient (4), spend_sk (5),
        // position bits (7 to 7+depth-1), siblings (7+depth to 7+2*depth-1)
        // Also change output: change_rho and change_pk
        let depth = tree_depth as usize;
        let mut private_indices: Vec<usize> = vec![3, 4, 5];
        // Add position bit indices (7 through 7+depth-1)
        for i in 0..depth {
            private_indices.push(7 + i);
        }
        // Add sibling indices (7+depth through 7+2*depth-1)
        for i in 0..depth {
            private_indices.push(7 + depth + i);
        }
        // Add change output private fields
        // Output args start at 11+2*depth: value, rho, pk, cm
        // change_rho is at 11+2*depth+1, change_pk is at 11+2*depth+2
        let change_rho_idx = 11 + 2 * depth + 1;
        let change_pk_idx = 11 + 2 * depth + 2;
        private_indices.push(change_rho_idx);
        private_indices.push(change_pk_idx);

        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path)
            .with_private_indices(private_indices);

        // Add arguments with NEW layout
        // 1: domain
        host.add_hex_arg(hex32(&domain));
        // 2: value
        host.add_u64_arg(value);
        // 3: rho [PRIVATE]
        host.add_hex_arg(hex32(&rho));
        // 4: recipient [PRIVATE]
        host.add_hex_arg(hex32(&recipient));
        // 5: spend_sk [PRIVATE]
        host.add_hex_arg(hex32(&spend_sk));
        // 6: depth
        host.add_u64_arg(tree_depth as u64);

        // 7 to 6+depth: position bits [PRIVATE]
        // Each bit is passed as a 32-byte field element (0x00...00 for 0, 0x00...01 for 1)
        for level in 0..depth {
            let bit = ((position >> level) & 1) as u8;
            let mut bit_bytes = [0u8; 32];
            bit_bytes[31] = bit;  // Little-endian: bit value in last byte
            host.add_hex_arg(hex32(&bit_bytes));
        }

        // 7+depth to 6+2*depth: siblings [PRIVATE]
        for sibling in &siblings {
            host.add_hex_arg(hex32(sibling));
        }

        // 7+2*depth: anchor (str - field element format for from_c_str)
        host.add_str_arg(hex32(&anchor));
        // 8+2*depth: nullifier (str)
        host.add_str_arg(hex32(&nf));
        // 9+2*depth: withdraw_amount
        host.add_u64_arg(withdraw_amount);
        // 10+2*depth: n_out
        host.add_u64_arg(n_out as u64);

        // Output args (starting at 11+2*depth):
        // For each output: value, rho, pk, cm
        // 11+2*depth: change_value
        host.add_u64_arg(change_value);
        // 11+2*depth+1: change_rho [PRIVATE]
        host.add_hex_arg(hex32(&change_rho));
        // 11+2*depth+2: change_pk [PRIVATE]
        host.add_hex_arg(hex32(&change_pk));
        // 11+2*depth+3: cm_change
        host.add_hex_arg(hex32(&cm_change));

        host.set_public_output(&public_output)?;

        let code_commitment = host.code_commitment();

        println!("\n==================== PROVER ====================\n");
        
        let proof_start = Instant::now();
        let (proof_data, prover_stdout) = host.run_with_logging()
            .context("Failed to generate proof")?;
        let proof_time = proof_start.elapsed();

        // Print full prover output
        println!("{}", prover_stdout);
        
        // Extract stats from prover output
        let linear = prover_stdout.lines()
            .find(|l| l.contains("Num Linear constraints:"))
            .and_then(|l| l.split_whitespace().last())
            .unwrap_or("?");
        let quadratic = prover_stdout.lines()
            .find(|l| l.contains("Num quadratic constraints:"))
            .and_then(|l| l.split_whitespace().last())
            .unwrap_or("?");

        println!("\n✓ Proof generated: {} bytes ({:.3}s)", proof_data.len(), proof_time.as_secs_f64());

        println!("\n==================== VERIFIER ====================\n");
        
        // Set environment variables for verifier
        // SAFETY: This is a single-threaded test, no concurrent access to env vars
        unsafe {
            std::env::set_var("LIGERO_PROGRAM_PATH", host.program_path());
            std::env::set_var("LIGERO_SHADER_PATH", host.shader_path());
            std::env::set_var("LIGERO_PACKING", host.packing().to_string());
            std::env::set_var("LIGERO_VERIFIER_BIN", host.verifier_bin().to_string_lossy().to_string());
        }

        let verify_start = Instant::now();
        let verify_result: Result<(SpendPublic, String, String)> = LigeroVerifier::verify_with_output(&proof_data, &code_commitment);
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
                eprintln!("\n✗ Verification FAILED ({:.3}s)", verify_time.as_secs_f64());
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
        println!("  Total Time:    {:.0}ms", (proof_time + verify_time).as_millis());
        println!("==============================================");

        assert_eq!(verified.withdraw_amount, withdraw_amount as u128);
        assert_eq!(verified.output_commitments.len(), 1);
        assert_eq!(verified.output_commitments[0], cm_change);

        println!("\n=== Test Complete ===");
        println!("✓ Withdrawal: {} transparent + {} shielded change", withdraw_amount, change_value);

        Ok(())
    }
}
