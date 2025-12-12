//! Verify the relationship between privacy address and recipient hash
//!
//! This test checks that a given privacy address correctly converts to the expected recipient hash.

use mcp_external::privacy_key::PrivacyKey;

/// Domain constant matching the one used in deposit.rs
const DOMAIN: [u8; 32] = [1u8; 32];

#[test]
fn test_privacy_address_to_recipient() {
    // The privacy address provided by the user
    let privacy_address = "privpool1eqrexjkvvw5wjljp4mpup250hl4sdpk6hl36dmdcdsfldvjw2j8staydzl";

    // Parse the privacy address
    let key_from_address =
        PrivacyKey::from_address(privacy_address).expect("Failed to parse privacy address");

    // Derive the recipient from the privacy address
    let recipient = key_from_address.recipient(&DOMAIN);
    let recipient_hex = hex::encode(&recipient);

    println!("✅ Privacy Address to Recipient Conversion:");
    println!("   Privacy Address: {}", privacy_address);
    println!("   Recipient Hash:  {}", recipient_hex);
    println!();
    println!("Expected recipient in deposit response: {}", recipient_hex);

    // The recipient you saw in the response
    let expected_recipient = "aa679508d91fabf66b009c037144534ade270c849dd1bf110bf1e7d8706a6a66";

    println!();
    println!("Comparison:");
    println!("   From privacy address: {}", recipient_hex);
    println!("   From deposit response: {}", expected_recipient);

    if recipient_hex == expected_recipient {
        println!("   ✅ MATCH! The recipient is correctly derived from your privacy address.");
    } else {
        println!("   ❌ MISMATCH! The recipient does not match.");
        println!();
        println!("This could mean:");
        println!("1. The deposit was made with a different privacy key");
        println!("2. The domain differs between address derivation and deposit");
        println!("3. The privacy address was entered incorrectly");
    }

    // Assert they match
    assert_eq!(
        recipient_hex, expected_recipient,
        "Recipient should match the one derived from privacy address"
    );
}

#[test]
fn test_reverse_engineer_privacy_key() {
    // If we know the recipient, can we verify it matches a privacy address?
    let recipient_from_deposit = "aa679508d91fabf66b009c037144534ade270c849dd1bf110bf1e7d8706a6a66";

    // Try the privacy address from the user
    let privacy_address = "privpool1eqrexjkvvw5wjljp4mpup250hl4sdpk6hl36dmdcdsfldvjw2j8staydzl";
    let key = PrivacyKey::from_address(privacy_address).expect("Failed to parse privacy address");

    let derived_recipient = hex::encode(&key.recipient(&DOMAIN));

    println!("✅ Verification Test:");
    println!("   Privacy Address:       {}", privacy_address);
    println!("   Derived Recipient:     {}", derived_recipient);
    println!("   Expected from Deposit: {}", recipient_from_deposit);
    println!();

    if derived_recipient == recipient_from_deposit {
        println!("   ✅ CONFIRMED: This privacy address owns the deposited note!");
        println!("   You will be able to spend this note with the corresponding spend_sk.");
    } else {
        println!("   ❌ WARNING: This privacy address does NOT match the deposit recipient!");
        println!("   The note was deposited to a different address.");
    }
}

#[test]
fn test_show_privacy_address_components() {
    let privacy_address = "privpool1eqrexjkvvw5wjljp4mpup250hl4sdpk6hl36dmdcdsfldvjw2j8staydzl";

    // Parse the privacy address to get the public key
    let key = PrivacyKey::from_address(privacy_address).expect("Failed to parse privacy address");

    // Get the public key (pk_out)
    let pk = key.pk();
    let pk_hex = hex::encode(pk);

    // Get the derived recipient
    let recipient = key.recipient(&DOMAIN);
    let recipient_hex = hex::encode(&recipient);

    println!("✅ Privacy Address Components:");
    println!("   Privacy Address: {}", privacy_address);
    println!("   Public Key (pk): {}", pk_hex);
    println!("   Recipient:       {}", recipient_hex);
    println!();
    println!("How it works:");
    println!("   1. Privacy address (bech32m) → Public Key (pk)");
    println!("   2. pk + domain → Recipient (via H('ADDR_V1' || domain || pk))");
    println!("   3. Recipient is what's stored in the note commitment on-chain");
}
