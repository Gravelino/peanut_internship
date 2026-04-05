use ethers::types::Bytes;
use peanut_internship_rust::{Address, ChainClient, TokenAmount, TransactionBuilder, WalletManager, MIN_GAS_LIMIT, MAINNET_CHAIN_ID};

const TEST_RPC_URL: &str = "http://localhost:8545";
const TEST_TIMEOUT: u64 = 5;
const TEST_RETRIES: usize = 0;
const TEST_RECIPIENT: &str = "0x0000000000000000000000000000000000000001";

fn setup() -> (ChainClient, WalletManager) {
    let client = ChainClient::new(vec![TEST_RPC_URL.to_string()], TEST_TIMEOUT, TEST_RETRIES);
    let wallet = WalletManager::generate().unwrap();
    (client, wallet)
}

fn test_address() -> Address {
    Address::new(TEST_RECIPIENT).unwrap()
}

#[tokio::test]
async fn builder_requires_destination() {
    let (client, wallet) = setup();

    let result = TransactionBuilder::new(client, wallet)
        .nonce(0)
        .gas_limit(MIN_GAS_LIMIT)
        .build()
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn builder_uses_default_eth_zero() {
    let (client, wallet) = setup();
    let to = test_address();

    let tx = TransactionBuilder::new(client, wallet)
        .to(to)
        .nonce(0)
        .gas_limit(MIN_GAS_LIMIT)
        .build()
        .await;

    assert!(tx.is_ok());
    assert_eq!(tx.unwrap().value, TokenAmount::eth(0));
}

#[tokio::test]
async fn builder_uses_default_mainnet_chain_id() {
    let (client, wallet) = setup();
    let to = test_address();

    let tx = TransactionBuilder::new(client, wallet)
        .to(to)
        .nonce(0)
        .gas_limit(MIN_GAS_LIMIT)
        .build()
        .await
        .unwrap();

    assert_eq!(tx.chain_id, MAINNET_CHAIN_ID);
}

#[tokio::test]
async fn builder_preserves_custom_data() {
    let (client, wallet) = setup();
    let to = test_address();
    let data = vec![1, 2, 3, 4];

    let tx = TransactionBuilder::new(client, wallet)
        .to(to)
        .data(data.clone())
        .nonce(0)
        .gas_limit(MIN_GAS_LIMIT)
        .build()
        .await
        .unwrap();

    assert_eq!(tx.data, Bytes::from(data));
}

#[tokio::test]
async fn builder_preserves_custom_nonce() {
    let (client, wallet) = setup();
    let to = test_address();

    let tx = TransactionBuilder::new(client, wallet)
        .to(to)
        .nonce(42)
        .gas_limit(MIN_GAS_LIMIT)
        .build()
        .await
        .unwrap();

    assert_eq!(tx.nonce, Some(42));
}

#[tokio::test]
async fn builder_builds_with_explicit_fields() {
    let (client, wallet) = setup();
    let to = test_address();
    let amount = TokenAmount::from_eth("0.001").unwrap();

    let tx = TransactionBuilder::new(client, wallet)
        .to(to.clone())
        .value(amount.clone())
        .nonce(0)
        .gas_limit(MIN_GAS_LIMIT)
        .chain_id(11155111)
        .build()
        .await
        .unwrap();

    assert_eq!(tx.to, to);
    assert_eq!(tx.value, amount);
    assert_eq!(tx.nonce, Some(0));
    assert_eq!(tx.gas_limit, Some(MIN_GAS_LIMIT));
    assert_eq!(tx.chain_id, 11155111);
}

#[tokio::test]
async fn builder_signs_transaction_bytes() {
    let (client, wallet) = setup();
    let to = test_address();

    let signed = TransactionBuilder::new(client, wallet)
        .to(to)
        .value(TokenAmount::eth(1u64))
        .nonce(0)
        .gas_limit(MIN_GAS_LIMIT)
        .chain_id(11155111)
        .build_and_sign()
        .await
        .unwrap();

    assert!(!signed.is_empty());
}
