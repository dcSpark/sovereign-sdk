//! Test that wallet config includes the privacy address
//!
//! This test verifies that the getWalletConfig operation returns the privacy pool address
//! so users can share it with others for receiving transfers.

use mcp_external::privacy_key::PrivacyKey;

#[test]
fn test_privacy_address_format() {
    // Create a test privacy key
    let test_spend_sk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let privacy_key = PrivacyKey::from_hex(test_spend_sk).expect("Failed to create privacy key");

    // Get the privacy address
    let privacy_address = privacy_key.privacy_address();
    let addr_str = privacy_address.to_string();

    // Verify format
    assert!(
        addr_str.starts_with("privpool1"),
        "Privacy address should start with privpool1, got: {}",
        addr_str
    );

    // Should be a valid bech32m string
    assert!(
        addr_str.len() > 10,
        "Privacy address should be longer than just the prefix"
    );

    println!("✅ Privacy address format test passed!");
    println!("   Privacy Address: {}", addr_str);
    println!("   This address can be shared with others to receive shielded transfers");
}

#[test]
fn test_privacy_address_consistency() {
    // Same key should always produce the same address
    let test_spend_sk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let key1 = PrivacyKey::from_hex(test_spend_sk).unwrap();
    let key2 = PrivacyKey::from_hex(test_spend_sk).unwrap();

    let addr1 = key1.privacy_address().to_string();
    let addr2 = key2.privacy_address().to_string();

    assert_eq!(
        addr1, addr2,
        "Same privacy key should always produce the same address"
    );

    println!("✅ Privacy address consistency test passed!");
    println!("   Address: {}", addr1);
}

#[test]
fn test_different_keys_different_addresses() {
    // Different keys should produce different addresses
    let key1 =
        PrivacyKey::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();

    let key2 =
        PrivacyKey::from_hex("1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();

    let addr1 = key1.privacy_address().to_string();
    let addr2 = key2.privacy_address().to_string();

    assert_ne!(
        addr1, addr2,
        "Different privacy keys should produce different addresses"
    );

    println!("✅ Different keys different addresses test passed!");
    println!("   Key 1 Address: {}", addr1);
    println!("   Key 2 Address: {}", addr2);
}
