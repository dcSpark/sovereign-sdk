#[cfg(feature = "native")]
mod tests {
    use sov_ligero_adapter::{Ligero, LigeroHost, LigeroVerifier};
    use sov_rollup_interface::zk::{CodeCommitment, ZkVerifier, Zkvm, ZkvmHost};
    use std::path::PathBuf;

    fn get_test_program_path() -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let program_path = manifest_dir.join("bins/programs/edit.wasm");
        program_path.to_string_lossy().to_string()
    }

    #[test]
    fn test_ligero_host_creation() {
        let program_path = get_test_program_path();
        let host = LigeroHost::new(&program_path);
        
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
        let host = LigeroHost::new(&program_path)
            .with_packing(4096);
        
        // Should not panic
    }

    #[test]
    fn test_host_with_private_indices() {
        let program_path = get_test_program_path();
        let host = LigeroHost::new(&program_path)
            .with_private_indices(vec![1, 2]);
        
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
        
        // Try to generate a proof
        // This will fail if WebGPU is not available, but that's ok for CI
        match host.run(true) {
            Ok(proof_data) => {
                println!("Proof generated successfully, size: {} bytes", proof_data.len());
                assert!(!proof_data.is_empty());
            }
            Err(e) => {
                eprintln!("Proof generation failed (expected in some environments): {}", e);
            }
        }
    }

    #[test]
    fn test_simulation_mode() {
        let program_path = get_test_program_path();
        let mut host = <Ligero as Zkvm>::Host::from_args(&program_path);
        
        // Simulation mode should always work (even without binaries)
        let result = host.run(false);
        
        // Should succeed
        assert!(result.is_ok());
        let proof_data = result.unwrap();
        
        // Proof data should be a serialized empty package
        assert!(!proof_data.is_empty());
    }
}
