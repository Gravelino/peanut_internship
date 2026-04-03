use std::collections::HashSet;

use peanut_internship_rust::{Address, CanonicalSerializer, Token, TokenAmount, WalletManager, TransactionRequest};
use ethers::types::{Bytes, U256};
use rust_decimal::Decimal;

#[test]
fn address_validation_and_equality_are_case_insensitive() {
    let lower = Address::new("0x52908400098527886E0F7030069857D2E4169EE7").unwrap();
    let mixed = Address::new("0x52908400098527886e0f7030069857d2e4169ee7").unwrap();

    assert_eq!(lower, mixed);
    assert_eq!(lower.checksum(), mixed.checksum());
}

#[test]
fn invalid_address_is_rejected() {
    let error = Address::new("invalid").unwrap_err();
    assert!(error.to_string().contains("invalid Ethereum address"));
}

#[test]
fn token_amount_from_human_uses_integer_scaling() {
    let amount = TokenAmount::from_human("1.5", 18, Some("ETH".to_string())).unwrap();
    assert_eq!(amount.raw.to_string(), "1500000000000000000");
    assert_eq!(amount.human(), Decimal::new(15, 1));
}

#[test]
fn token_amount_arithmetic_requires_matching_decimals() {
    let left = TokenAmount::from_human("1", 18, None).unwrap();
    let right = TokenAmount::from_human("1", 6, None).unwrap();

    assert!(left.checked_add(&right).is_err());
}

#[test]
fn token_identity_depends_only_on_address() {
    let address = Address::new("0x0000000000000000000000000000000000000001").unwrap();
    let token_a = Token {
        address: address.clone(),
        symbol: "AAA".to_string(),
        decimals: 18,
    };
    let token_b = Token {
        address,
        symbol: "BBB".to_string(),
        decimals: 6,
    };

    assert_eq!(token_a, token_b);

    let mut set = HashSet::new();
    set.insert(token_a);
    set.insert(token_b);
    assert_eq!(set.len(), 1);
}

#[test]
fn canonical_serializer_is_deterministic() {
    let payload = serde_json::json!({
        "z": [3, 2, 1],
        "a": {
            "b": "text",
            "a": "emoji 🚀"
        }
    });

    let first = CanonicalSerializer::serialize(&payload).unwrap();
    let second = CanonicalSerializer::serialize(&payload).unwrap();

    assert_eq!(first, second);
    assert!(CanonicalSerializer::verify_determinism(&payload, 10).unwrap());
}

#[test]
fn canonical_serializer_rejects_floats() {
    let payload = serde_json::json!({"value": 1.25});
    assert!(CanonicalSerializer::serialize(&payload).is_err());
}

#[test]
fn wallet_repr_does_not_expose_private_key() {
    let wallet = WalletManager::generate().unwrap();
    let debug = format!("{:?}", wallet);
    let display = format!("{}", wallet);

    assert!(debug.contains("WalletManager"));
    assert!(display.contains("WalletManager(address="));
    assert!(!debug.to_lowercase().contains("private key"));
    assert!(!display.to_lowercase().contains("private key"));
}

#[test]
fn empty_message_is_rejected_before_signing() {
    let wallet = WalletManager::generate().unwrap();
    let error = wallet.sign_message("").unwrap_err();
    assert!(error.to_string().contains("must not be empty"));
}

#[test]
fn oversized_message_is_rejected_before_crypto() {
    let wallet = WalletManager::generate().unwrap();
    let huge_message = "x".repeat(2_000_000);
    let error = wallet.sign_message(&huge_message).unwrap_err();
    assert!(error.to_string().contains("exceeds maximum size"));
}

#[test]
fn exception_messages_do_not_leak_sensitive_data() {
    let wallet = WalletManager::generate().unwrap();

    let result = wallet.sign_message("test");
    assert!(result.is_ok());

    let error = WalletManager::from_env("NONEXISTENT_VAR_12345").unwrap_err();
    let error_str = error.to_string();

    assert!(error_str.contains("NONEXISTENT_VAR_12345") || error_str.contains("missing"));
    assert!(!error_str.contains("0x"));
    assert!(error_str.len() < 500);
}

#[test]
fn transaction_validation_before_signing() {
    let wallet = WalletManager::generate().unwrap();
    let recipient = Address::new("0x52908400098527886E0F7030069857D2E4169EE7").unwrap();

    let valid_tx = TransactionRequest {
        to: recipient.clone(),
        value: TokenAmount::from_human("0", 18, None).unwrap(),
        data: Bytes::new(),
        nonce: Some(0),
        gas_limit: Some(21000),
        max_fee_per_gas: Some(U256::from(1_000_000_000u64)),
        max_priority_fee: Some(U256::from(1_000_000_000u64)),
        chain_id: 1,
    };

    assert!(wallet.sign_transaction(&valid_tx).is_ok());

    let invalid_tx = TransactionRequest {
        to: recipient.clone(),
        value: TokenAmount::from_human("0", 18, None).unwrap(),
        data: Bytes::new(),
        nonce: Some(0),
        gas_limit: Some(21000),
        max_fee_per_gas: Some(U256::from(1_000_000_000u64)),
        max_priority_fee: Some(U256::from(2_000_000_000u64)),
        chain_id: 1,
    };
    
    let error = wallet.sign_transaction(&invalid_tx).unwrap_err();
    assert!(error.to_string().contains("maxPriorityFeePerGas"));
}

#[test]
fn canonical_serializer_handles_null_values() {
    let payload = serde_json::json!({
        "a": null,
        "b": [null, 1, null],
        "c": {"nested": null}
    });

    let result = CanonicalSerializer::serialize(&payload);
    assert!(result.is_ok());

    let bytes = result.unwrap();
    let reparsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(reparsed["a"].is_null());
    assert!(reparsed["b"][0].is_null());
}

#[test]
fn canonical_serializer_handles_empty_objects_and_arrays() {
    let payload = serde_json::json!({
        "empty_obj": {},
        "empty_arr": [],
        "nested": {"inner": {}, "list": []}
    });

    let result = CanonicalSerializer::serialize(&payload);
    assert!(result.is_ok());

    let bytes = result.unwrap();
    let reparsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reparsed["empty_obj"], serde_json::json!({}));
    assert_eq!(reparsed["empty_arr"], serde_json::json!([]));
}

#[test]
fn canonical_serializer_handles_large_integers() {
    let payload = serde_json::json!({"big": 9007199254740993_i64});
    let result = CanonicalSerializer::serialize(&payload);
    assert!(result.is_ok());

    let bytes = result.unwrap();
    let reparsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reparsed["big"].as_i64().unwrap(), 9007199254740993_i64);
}

#[test]
fn canonical_serializer_determinism_1000_iterations() {
    let payload = serde_json::json!({
        "z": [3, 2, 1],
        "a": {"b": "text", "a": "emoji 🚀"},
        "m": null,
        "empty": {},
        "list": []
    });

    assert!(CanonicalSerializer::verify_determinism(&payload, 1000).unwrap());
}

#[test]
fn tokens_with_different_addresses_are_not_equal() {
    let token_a = Token {
        address: Address::new("0x0000000000000000000000000000000000000001").unwrap(),
        symbol: "AAA".to_string(),
        decimals: 18,
    };
    let token_b = Token {
        address: Address::new("0x0000000000000000000000000000000000000002").unwrap(),
        symbol: "AAA".to_string(),
        decimals: 18,
    };

    assert_ne!(token_a, token_b);

    let mut set = HashSet::new();
    set.insert(token_a);
    set.insert(token_b);
    assert_eq!(set.len(), 2);
}
