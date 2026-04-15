use ethers::types::Bytes;
use peanut_internship_rust::{
    Address, MAINNET_CHAIN_ID, MIN_GAS_LIMIT, TokenAmount, WalletManager,
};
use std::fs;
use std::path::PathBuf;

const TEST_PASSWORD: &str = "test_password_secure_123";
const TEST_RECIPIENT: &str = "0x52908400098527886E0F7030069857D2E4169EE7";
const TEST_RECIPIENT_ALT: &str = "0x70997970C51812e339D9B73b0245ad59E6f629E9";

fn test_keystore_path() -> PathBuf {
    let path = PathBuf::from("target/test_keystores");
    let _ = fs::create_dir_all(&path);
    path.join(format!("test_keystore_{}.json", std::process::id()))
}

fn test_address() -> Address {
    Address::new(TEST_RECIPIENT).unwrap()
}

fn test_address_alt() -> Address {
    Address::new(TEST_RECIPIENT_ALT).unwrap()
}

#[test]
fn repr_display_never_expose_private_key() {
    let wallet = WalletManager::generate().unwrap();

    let debug_str = format!("{:?}", wallet);
    assert!(debug_str.contains("WalletManager"));
    assert!(!debug_str.to_lowercase().contains("secret"));
    assert!(!debug_str.to_lowercase().contains("key"));

    let display_str = format!("{}", wallet);
    assert!(display_str.contains("WalletManager(address="));
    assert!(!display_str.to_lowercase().contains("secret"));
}

#[tokio::test]
async fn empty_message_rejected_before_crypto() {
    let wallet = WalletManager::generate().unwrap();
    let result = wallet.sign_message("").await;

    assert!(result.is_err());
    let error = result.unwrap_err();
    assert_eq!(error.to_string(), "message must not be empty");
}

#[tokio::test]
async fn message_size_validation_before_crypto() {
    let wallet = WalletManager::generate().unwrap();

    let huge_msg = "A".repeat(1_000_001);
    let result = wallet.sign_message(&huge_msg).await;

    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.to_string().contains("exceeds maximum size"));
    assert!(error.to_string().contains("1000000"));
}

#[tokio::test]
async fn normal_messages_sign_successfully() {
    let wallet = WalletManager::generate().unwrap();

    let sig1 = wallet.sign_message("Hello, Ethereum!").await.unwrap();
    assert!(!sig1.to_string().is_empty());

    let big_msg = "B".repeat(1_000_000);
    let sig2 = wallet.sign_message(&big_msg).await.unwrap();
    assert!(!sig2.to_string().is_empty());
}

#[test]
fn error_messages_sanitize_sensitive_data() {
    let result = WalletManager::from_env("NONEXISTENT_VAR_xyz123");
    assert!(result.is_err());

    let error_msg = result.unwrap_err().to_string();

    assert!(error_msg.contains("NONEXISTENT_VAR_xyz123"));

    assert!(!error_msg.contains("0xf"));
    assert!(!error_msg.contains("0xF"));
}

#[tokio::test]
async fn transaction_validation_rejects_invalid_fees() {
    let wallet = WalletManager::generate().unwrap();
    let to = test_address();

    let tx_request = peanut_internship_rust::TransactionRequest {
        to,
        value: TokenAmount::eth(0),
        data: Bytes::new(),
        nonce: Some(0),
        gas_limit: Some(MIN_GAS_LIMIT),
        max_fee_per_gas: Some(ethers::types::U256::from(1_000_000_000u64)),
        max_priority_fee: Some(ethers::types::U256::from(2_000_000_000u64)),
        chain_id: MAINNET_CHAIN_ID,
    };

    let result = wallet.sign_transaction(&tx_request).await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("maxPriorityFeePerGas")
    );
}

#[tokio::test]
async fn transaction_validation_rejects_zero_chain_id() {
    let wallet = WalletManager::generate().unwrap();
    let to = test_address();

    let tx_request = peanut_internship_rust::TransactionRequest {
        to,
        value: TokenAmount::eth(0),
        data: Bytes::new(),
        nonce: None,
        gas_limit: Some(MIN_GAS_LIMIT),
        max_fee_per_gas: Some(ethers::types::U256::from(1_000_000_000u64)),
        max_priority_fee: None,
        chain_id: 0,
    };

    let result = wallet.sign_transaction(&tx_request).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("chain_id"));
}

#[test]
fn keyfile_export_creates_encrypted_file() {
    let wallet = WalletManager::generate().unwrap();
    let test_dir = std::env::temp_dir().join("peanut_tests_export");

    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&test_dir).expect("failed to create test dir");

    let result = wallet.to_keyfile(&test_dir, TEST_PASSWORD, Some("test_wallet".to_string()));
    assert!(
        result.is_ok(),
        "Failed to export keyfile: {:?}",
        result.unwrap_err()
    );

    let entries: Vec<_> = fs::read_dir(&test_dir)
        .expect("Cannot read directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();

    assert!(
        !entries.is_empty(),
        "No files created in keystore directory"
    );

    let mut found_valid = false;
    for file_path in entries {
        if let Ok(contents) = fs::read_to_string(&file_path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents)
        {
            let has_crypto = json.get("crypto").is_some() || json.get("Crypto").is_some();
            if json.get("address").is_some() && has_crypto {
                found_valid = true;
                break;
            }
        }
    }
    assert!(
        found_valid,
        "No valid keystore JSON file found in directory"
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[tokio::test]
async fn keyfile_import_decrypts_correctly() {
    let wallet1 = WalletManager::generate().unwrap();
    let addr1 = wallet1.address();

    let test_dir = std::env::temp_dir().join("peanut_tests_import");
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&test_dir).expect("failed to create test dir");

    wallet1
        .to_keyfile(&test_dir, TEST_PASSWORD, Some("wallet1".to_string()))
        .expect("failed to export keyfile");

    let actual_file = fs::read_dir(&test_dir)
        .expect("Cannot read directory")
        .find_map(|entry| {
            entry.ok().and_then(|e| {
                let path = e.path();
                if path.is_file() { Some(path) } else { None }
            })
        })
        .expect("No keystore file found");

    let wallet2 = WalletManager::from_keyfile(&actual_file, TEST_PASSWORD)
        .expect("Failed to import wallet from keyfile");

    assert_eq!(
        wallet2.address(),
        addr1,
        "Imported wallet has different address"
    );

    let message = "Test message for signature verification";
    let sig1 = wallet1.sign_message(message).await.unwrap();
    let sig2 = wallet2.sign_message(message).await.unwrap();

    assert_eq!(
        sig1.to_string(),
        sig2.to_string(),
        "Signatures do not match"
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn keyfile_wrong_password_fails_gracefully() {
    let wallet = WalletManager::generate().unwrap();

    let test_dir = std::env::temp_dir().join("peanut_tests_wrong_pass");
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&test_dir).expect("failed to create test dir");

    wallet
        .to_keyfile(&test_dir, TEST_PASSWORD, Some("wallet2".to_string()))
        .expect("failed to export keyfile");

    let actual_file = fs::read_dir(&test_dir)
        .expect("Cannot read directory")
        .find_map(|entry| {
            entry.ok().and_then(|e| {
                let path = e.path();
                if path.is_file() { Some(path) } else { None }
            })
        })
        .expect("No keystore file found");

    let result = WalletManager::from_keyfile(&actual_file, "wrong_password");

    assert!(result.is_err(), "Should fail with wrong password");
    let error_msg = result.unwrap_err().to_string();
    assert!(
        error_msg.to_lowercase().contains("decrypt") || error_msg.to_lowercase().contains("failed"),
        "Error should mention decryption or failure. Got: {}",
        error_msg
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn keyfile_missing_file_fails_gracefully() {
    let path = PathBuf::from("target/nonexistent_keystore_xyz.json");
    let _ = fs::remove_file(&path);

    let result = WalletManager::from_keyfile(&path, "password");

    assert!(result.is_err(), "Should fail for missing file");
    let error_msg = result.unwrap_err().to_string();
    assert!(
        error_msg.contains("read keyfile") || error_msg.contains("No such file"),
        "Error should mention file reading"
    );
}

#[test]
fn keyfile_corrupted_json_fails_gracefully() {
    let path = test_keystore_path();
    let dir = path.parent().unwrap();

    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();

    let corrupted_file = dir.join("corrupted.json");
    fs::write(&corrupted_file, "{ this is not valid json }").unwrap();

    let result = WalletManager::from_keyfile(&corrupted_file, TEST_PASSWORD);

    assert!(result.is_err(), "Should fail for corrupted JSON");
    let error_msg = result.unwrap_err().to_string();
    assert!(
        error_msg.to_lowercase().contains("keyfile")
            || error_msg.to_lowercase().contains("invalid")
            || error_msg.to_lowercase().contains("failed"),
        "Error should mention keyfile, invalid format, or failure. Got: {}",
        error_msg
    );

    let _ = fs::remove_dir_all(dir);
}

#[tokio::test]
async fn keyfile_roundtrip_preserves_functionality() {
    let wallet_orig = WalletManager::generate().unwrap();
    let recipient = test_address_alt();

    let tx = peanut_internship_rust::TransactionRequest {
        to: recipient.clone(),
        value: TokenAmount::from_eth("1.5").unwrap(),
        data: Bytes::new(),
        nonce: Some(42),
        gas_limit: Some(MIN_GAS_LIMIT),
        max_fee_per_gas: Some(ethers::types::U256::from(1_000_000_000u64)),
        max_priority_fee: None,
        chain_id: MAINNET_CHAIN_ID,
    };

    let sig_orig = wallet_orig.sign_transaction(&tx).await.unwrap();

    let test_dir = std::env::temp_dir().join("peanut_tests_roundtrip");
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&test_dir).expect("failed to create test dir");

    wallet_orig
        .to_keyfile(&test_dir, TEST_PASSWORD, Some("wallet3".to_string()))
        .expect("failed to export keyfile");

    let actual_file = fs::read_dir(&test_dir)
        .expect("Cannot read directory")
        .find_map(|entry| {
            entry.ok().and_then(|e| {
                let path = e.path();
                if path.is_file() { Some(path) } else { None }
            })
        })
        .expect("No keystore file found");

    let wallet_reimport =
        WalletManager::from_keyfile(&actual_file, TEST_PASSWORD).expect("failed to import keyfile");

    let sig_reimport = wallet_reimport.sign_transaction(&tx).await.unwrap();

    assert_eq!(
        sig_orig.to_string(),
        sig_reimport.to_string(),
        "Reimported wallet produces different signature"
    );

    let _ = fs::remove_dir_all(&test_dir);
}
