#[cfg(test)]
mod tests {
    use midnight_privacy::{
        mt_combine, note_commitment, nullifier, poseidon2_hash, root_from_path, NullifierKey,
    };

    #[test]
    fn test_poseidon2_hash_deterministic() {
        // Same input should always produce same output
        let input1 = b"test_input";
        let input2 = b"test_input";

        let hash1 = poseidon2_hash(b"TEST_TAG", &[input1]);
        let hash2 = poseidon2_hash(b"TEST_TAG", &[input2]);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_poseidon2_hash_different_inputs() {
        // Different inputs should produce different outputs
        let hash1 = poseidon2_hash(b"TEST_TAG", &[b"input1"]);
        let hash2 = poseidon2_hash(b"TEST_TAG", &[b"input2"]);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_domain_separation() {
        // Same input with different domain tags should produce different outputs
        let input = b"test_input";
        let hash1 = poseidon2_hash(b"DOMAIN_A", &[input]);
        let hash2 = poseidon2_hash(b"DOMAIN_B", &[input]);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_note_commitment() {
        let domain = [1u8; 32];
        let value = 100u128;
        let rho = [2u8; 32];
        let recipient = [3u8; 32];

        let cm1 = note_commitment(&domain, value, &rho, &recipient);
        let cm2 = note_commitment(&domain, value, &rho, &recipient);

        // Same inputs should produce same commitment
        assert_eq!(cm1, cm2);

        // Different value should produce different commitment
        let cm3 = note_commitment(&domain, value + 1, &rho, &recipient);
        assert_ne!(cm1, cm3);
    }

    #[test]
    fn test_nullifier() {
        // PRF-based nullifier: nf = Poseidon2("PRF_NF_V1" || domain || nf_key || rho)
        let domain = [1u8; 32];
        let nf_key = [2u8; 32];
        let rho = [3u8; 32];

        let nf1 = nullifier(&domain, &nf_key, &rho);
        let nf2 = nullifier(&domain, &nf_key, &rho);

        // Same inputs should produce same nullifier
        assert_eq!(nf1, nf2);

        // Different rho should produce different nullifier
        let rho2 = [4u8; 32];
        let nf3 = nullifier(&domain, &nf_key, &rho2);
        assert_ne!(nf1, nf3);

        // Nullifier is now position-agnostic (no position in the computation)
        // This is the key property for parallel transaction safety
    }

    #[test]
    fn test_mt_combine() {
        let left = [1u8; 32];
        let right = [2u8; 32];
        let level = 0;

        let parent1 = mt_combine(level, &left, &right);
        let parent2 = mt_combine(level, &left, &right);

        // Same inputs should produce same parent
        assert_eq!(parent1, parent2);

        // Different level should produce different parent (prevents cross-level collisions)
        let parent3 = mt_combine(level + 1, &left, &right);
        assert_ne!(parent1, parent3);

        // Swapping left/right should produce different parent
        let parent4 = mt_combine(level, &right, &left);
        assert_ne!(parent1, parent4);
    }

    #[test]
    fn test_root_from_path() {
        // Simple 2-level tree test
        let leaf = [1u8; 32];
        let sibling0 = [2u8; 32];
        let sibling1 = [3u8; 32];

        let siblings = vec![sibling0, sibling1];
        let pos = 0u64; // leftmost position
        let depth = 2u8;

        // Manually compute expected root
        let level0_parent = mt_combine(0, &leaf, &sibling0);
        let expected_root = mt_combine(1, &level0_parent, &sibling1);

        // Should match root_from_path computation
        let computed_root = root_from_path(&leaf, pos, &siblings, depth);
        assert_eq!(expected_root, computed_root);
    }

    #[test]
    fn test_nullifier_key_display_parse() {
        let hash = [42u8; 32];
        let key = NullifierKey(hash);

        // Convert to string and back
        let s = key.to_string();
        let parsed: NullifierKey = s.parse().unwrap();

        assert_eq!(key, parsed);
    }
}
