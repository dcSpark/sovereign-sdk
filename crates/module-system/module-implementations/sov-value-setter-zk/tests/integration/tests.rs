#![cfg(feature = "native")]

use sov_modules_api::prelude::serde_json;
use sov_modules_api::Spec;
use sov_test_utils::TestSpec;
use sov_value_setter_zk::{Event, ValueProofPublic, ValueSetterZkConfig};

type S = TestSpec;

#[test]
fn test_config_serialization() {
    let admin = <S as Spec>::Address::from([1; 28]);
    let method_id = [42u8; 32];

    let config = ValueSetterZkConfig::<S> {
        admin: admin.clone(),
        method_id,
        initial_value: Some(50),
    };

    // Test that config can be serialized and deserialized
    let serialized = serde_json::to_string(&config).unwrap();
    let deserialized: ValueSetterZkConfig<S> = serde_json::from_str(&serialized).unwrap();

    assert_eq!(deserialized.admin, admin);
    assert_eq!(deserialized.method_id, method_id);
    assert_eq!(deserialized.initial_value, Some(50));
}

#[test]
fn test_config_without_initial_value() {
    let admin = <S as Spec>::Address::from([1; 28]);
    let method_id = [42u8; 32];

    let config = ValueSetterZkConfig::<S> {
        admin,
        method_id,
        initial_value: None,
    };

    assert_eq!(config.initial_value, None);
}

#[test]
fn test_value_proof_public_serialization() {
    let public = ValueProofPublic { value: 42 };

    // Test bincode serialization
    let serialized = bincode::serialize(&public).unwrap();
    let deserialized: ValueProofPublic = bincode::deserialize(&serialized).unwrap();

    assert_eq!(public, deserialized);
}

#[test]
fn test_event_serialization() {
    let event = Event::ValueSetWithProof { value: 42 };

    // Test that events can be serialized
    let serialized = borsh::to_vec(&event).unwrap();
    let deserialized: Event = borsh::from_slice(&serialized).unwrap();

    assert_eq!(event, deserialized);
}
