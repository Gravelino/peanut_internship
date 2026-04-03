use ethers::types::U256;
use peanut_internship_rust::{Address, ChainClient, TokenAmount, TransactionBuilder, WalletManager};

#[test]
fn builder_requires_destination() {
    let client = ChainClient::new(vec!["http://localhost:8545".to_string()], 5, 0);
    let wallet = WalletManager::generate().unwrap();

    let result = TransactionBuilder::new(client, wallet)
        .nonce(0)
        .gas_limit(21_000)
        .build();

    assert!(result.is_err());
}

#[test]
fn builder_builds_with_explicit_fields() {
    let client = ChainClient::new(vec!["http://localhost:8545".to_string()], 5, 0);
    let wallet = WalletManager::generate().unwrap();
    let to = Address::new("0x0000000000000000000000000000000000000001").unwrap();
    let amount = TokenAmount::from_human("0.001", 18, Some("ETH".to_string())).unwrap();

    let tx = TransactionBuilder::new(client, wallet)
        .to(to.clone())
        .value(amount.clone())
        .nonce(0)
        .gas_limit(21_000)
        .chain_id(11155111)
        .build()
        .unwrap();

    assert_eq!(tx.to, to);
    assert_eq!(tx.value, amount);
    assert_eq!(tx.nonce, Some(0));
    assert_eq!(tx.gas_limit, Some(21_000));
    assert_eq!(tx.chain_id, 11155111);
}

#[test]
fn builder_signs_transaction_bytes() {
    let client = ChainClient::new(vec!["http://localhost:8545".to_string()], 5, 0);
    let wallet = WalletManager::generate().unwrap();
    let to = Address::new("0x0000000000000000000000000000000000000001").unwrap();

    let signed = TransactionBuilder::new(client, wallet)
        .to(to)
        .value(TokenAmount {
            raw: U256::from(1u64),
            decimals: 18,
            symbol: Some("ETH".to_string()),
        })
        .nonce(0)
        .gas_limit(21_000)
        .chain_id(11155111)
        .build_and_sign()
        .unwrap();

    assert!(!signed.is_empty());
}
