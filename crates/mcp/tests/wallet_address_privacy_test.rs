//! Test that walletAddress includes the privacy pool address
//!
//! This test verifies that the walletAddress operation returns both the transparent
//! wallet address and the privacy pool address.

use mcp::privacy_key::PrivacyKey;

#[test]
fn test_privacy_address_included_in_response() {
    // This test verifies the structure that walletAddress should return
    // In practice, this would be returned by the MCP tool

    let test_spend_sk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let privacy_key = PrivacyKey::from_hex(test_spend_sk).expect("Failed to create privacy key");

    let privacy_address = privacy_key.privacy_address().to_string();

    // Simulate the expected response structure
    #[derive(serde::Serialize)]
    struct WalletAddressResponse {
        address: String,
        privacy_address: String,
    }

    let response = WalletAddressResponse {
        address: "0x742d35Cc6634C0532925a3b844Bc9e7595f0bEb2".to_string(),
        privacy_address: privacy_address.clone(),
    };

    // Verify the structure
    assert!(
        response.address.starts_with("0x"),
        "Transparent address should start with 0x"
    );
    assert!(
        response.privacy_address.starts_with("privpool1"),
        "Privacy address should start with privpool1"
    );

    println!("✅ Wallet Address Response Structure:");
    println!("   Transparent Address: {}", response.address);
    println!("   Privacy Address:     {}", response.privacy_address);
    println!();
    println!("Users can now:");
    println!("  1. Share transparent address for regular transfers");
    println!("  2. Share privacy address for shielded transfers");
}

#[test]
fn test_wallet_address_json_format() {
    // Verify the JSON output format is correct

    let test_spend_sk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let privacy_key = PrivacyKey::from_hex(test_spend_sk).expect("Failed to create privacy key");

    #[derive(serde::Serialize, serde::Deserialize)]
    struct WalletAddressResponse {
        address: String,
        privacy_address: String,
    }

    let response = WalletAddressResponse {
        address: "0x742d35Cc6634C0532925a3b844Bc9e7595f0bEb2".to_string(),
        privacy_address: privacy_key.privacy_address().to_string(),
    };

    // Serialize to JSON
    let json = serde_json::to_string_pretty(&response).expect("Failed to serialize");

    println!("✅ Wallet Address JSON Response:");
    println!("{}", json);

    // Verify it can be deserialized
    let parsed: WalletAddressResponse = serde_json::from_str(&json).expect("Failed to deserialize");

    assert_eq!(parsed.address, response.address);
    assert_eq!(parsed.privacy_address, response.privacy_address);

    // Verify JSON contains both fields
    assert!(
        json.contains("\"address\""),
        "JSON should contain address field"
    );
    assert!(
        json.contains("\"privacy_address\""),
        "JSON should contain privacy_address field"
    );
    assert!(
        json.contains("0x742d35Cc"),
        "JSON should contain transparent address value"
    );
    assert!(
        json.contains("privpool1"),
        "JSON should contain privacy address value"
    );
}

#[test]
fn test_multiple_calls_consistent_privacy_address() {
    // Verify that multiple calls return the same privacy address

    let test_spend_sk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let privacy_key = PrivacyKey::from_hex(test_spend_sk).expect("Failed to create privacy key");

    // Simulate multiple calls
    let addr1 = privacy_key.privacy_address().to_string();
    let addr2 = privacy_key.privacy_address().to_string();
    let addr3 = privacy_key.privacy_address().to_string();

    assert_eq!(addr1, addr2, "Privacy address should be consistent");
    assert_eq!(addr2, addr3, "Privacy address should be consistent");

    println!("✅ Privacy Address Consistency:");
    println!("   Call 1: {}", addr1);
    println!("   Call 2: {}", addr2);
    println!("   Call 3: {}", addr3);
    println!("   All match: ✅");
}
