use peanut_internship_rust::chain::client::classify_rpc_error;
use peanut_internship_rust::ChainError;

#[test]
fn classify_insufficient_funds() {
    let err = classify_rpc_error("execution reverted: insufficient funds for transfer");
    assert!(
        matches!(err, ChainError::InsufficientFunds),
        "Expected InsufficientFunds, got: {err:?}"
    );
}

#[test]
fn classify_insufficient_balance() {
    let err = classify_rpc_error("sender has insufficient balance");
    assert!(
        matches!(err, ChainError::InsufficientFunds),
        "Expected InsufficientFunds, got: {err:?}"
    );
}

#[test]
fn classify_nonce_too_low() {
    let err = classify_rpc_error("nonce too low");
    assert!(
        matches!(err, ChainError::NonceTooLow),
        "Expected NonceTooLow, got: {err:?}"
    );
}

#[test]
fn classify_nonce_already_used() {
    let err = classify_rpc_error("nonce has already been used");
    assert!(
        matches!(err, ChainError::NonceTooLow),
        "Expected NonceTooLow, got: {err:?}"
    );
}

#[test]
fn classify_replacement_underpriced() {
    let err = classify_rpc_error("replacement transaction underpriced");
    assert!(
        matches!(err, ChainError::ReplacementUnderpriced),
        "Expected ReplacementUnderpriced, got: {err:?}"
    );
}

#[test]
fn classify_already_known() {
    let err = classify_rpc_error("already known");
    assert!(
        matches!(err, ChainError::ReplacementUnderpriced),
        "Expected ReplacementUnderpriced, got: {err:?}"
    );
}

#[test]
fn classify_timeout() {
    let err = classify_rpc_error("request timed out after 30s");
    assert!(
        matches!(err, ChainError::Timeout),
        "Expected Timeout, got: {err:?}"
    );
}

#[test]
fn classify_generic_rpc_error() {
    let err = classify_rpc_error("some random RPC failure xyz");
    assert!(
        matches!(err, ChainError::Rpc(_)),
        "Expected generic Rpc, got: {err:?}"
    );
}

#[test]
fn client_with_zero_retries_fails_immediately() {
    use peanut_internship_rust::{Address, ChainClient};

    let client = ChainClient::new(vec!["http://127.0.0.1:1".to_string()], 1, 0);
    let address = Address::new("0x0000000000000000000000000000000000000001").unwrap();

    let result = client.get_balance(&address);
    assert!(result.is_err());
}

#[test]
fn client_retry_exhausts_all_urls() {
    use peanut_internship_rust::{Address, ChainClient};

    let client = ChainClient::new(
        vec![
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:2".to_string(),
        ],
        1,
        1,
    );
    let address = Address::new("0x0000000000000000000000000000000000000001").unwrap();

    let result = client.get_balance(&address);
    assert!(result.is_err());
}
