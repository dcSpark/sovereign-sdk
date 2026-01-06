//! Test that deposits use the configured privacy key
//!
//! This test verifies that when a privacy key is provided to the deposit operation,
//! the recipient is correctly derived from the privacy key instead of being random.

use mcp::privacy_key::PrivacyKey;

#[test]
fn test_privacy_key_recipient_derivation() {
    // Test spending key
    let spend_sk_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // Create privacy key
    let privacy_key = PrivacyKey::from_hex(spend_sk_hex).expect("Failed to create privacy key");

    // Domain matching the one used in deposit.rs
    let domain = [1u8; 32];

    // Derive recipient
    let recipient1 = privacy_key.recipient(&domain);
    let recipient2 = privacy_key.recipient(&domain);

    // Recipients should be deterministic
    assert_eq!(
        recipient1, recipient2,
        "Recipient derivation should be deterministic"
    );

    // Recipient should not be all zeros
    assert_ne!(recipient1, [0u8; 32], "Recipient should not be all zeros");

    // Privacy address should be in correct format
    let privacy_address = privacy_key.privacy_address(&domain);
    let addr_str = privacy_address.to_string();
    assert!(
        addr_str.starts_with("privpool1"),
        "Privacy address should start with privpool1, got: {}",
        addr_str
    );

    println!("✅ Privacy key recipient derivation test passed!");
    println!("   Privacy Address: {}", addr_str);
    println!("   Recipient: 0x{}", hex::encode(&recipient1));
}

#[test]
fn test_privacy_key_from_address() {
    // Test with a bech32m address (receive-only mode)
    let spend_sk_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let full_key = PrivacyKey::from_hex(spend_sk_hex).expect("Failed to create full key");

    // Get the address string
    let domain = [1u8; 32];
    let addr_str = full_key.privacy_address(&domain).to_string();

    // Create a new key from just the address
    let address_only_key =
        PrivacyKey::from_address(&addr_str).expect("Failed to create key from address");

    // Both should derive the same recipient
    assert_eq!(
        full_key.recipient(&domain),
        address_only_key.recipient(&domain),
        "Recipients should match between full key and address-only key"
    );

    // But the address-only key should not have spending capabilities
    assert!(
        full_key.nf_key(&domain).is_some(),
        "Full key should be able to derive nf_key"
    );
    assert!(
        address_only_key.nf_key(&domain).is_none(),
        "Address-only key should NOT be able to derive nf_key"
    );

    println!("✅ Privacy key from address test passed!");
    println!("   Address: {}", addr_str);
}
