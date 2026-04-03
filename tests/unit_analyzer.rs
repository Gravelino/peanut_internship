use peanut_internship_rust::chain::analyzer::{
    analyze_transaction, decode_event_topic, extract_selector, known_selectors,
};

#[test]
fn extract_selector_from_transfer_calldata() {
    let data = hex::decode("a9059cbb0000000000000000000000001234567890abcdef1234567890abcdef12345678").unwrap();
    assert_eq!(extract_selector(&data), Some("0xa9059cbb".to_string()));
}

#[test]
fn extract_selector_returns_none_for_short_input() {
    assert_eq!(extract_selector(&[0x01, 0x02, 0x03]), None);
    assert_eq!(extract_selector(&[]), None);
}

#[test]
fn extract_selector_works_with_exactly_4_bytes() {
    let data = vec![0x09, 0x5e, 0xa7, 0xb3];
    assert_eq!(extract_selector(&data), Some("0x095ea7b3".to_string()));
}

#[test]
fn known_selectors_contain_erc20_functions() {
    let map = known_selectors();
    assert!(map.get("0xa9059cbb").unwrap().starts_with("transfer"));
    assert!(map.get("0x095ea7b3").unwrap().starts_with("approve"));
    assert!(map.get("0x23b872dd").unwrap().starts_with("transferFrom"));
}

#[test]
fn known_selectors_contain_uniswap_v2_functions() {
    let map = known_selectors();
    assert!(map.contains_key("0x38ed1739"));
    assert!(map.contains_key("0x7ff36ab5"));
    assert!(map.contains_key("0x18cbafe5"));
    assert!(map.contains_key("0xe8e33700"));
    assert!(map.contains_key("0xbaa2abde"));
}

#[test]
fn known_selectors_contain_uniswap_v3_functions() {
    let map = known_selectors();
    assert!(map.contains_key("0xac9650d8"));
    assert!(map.contains_key("0x414bf389"));
    assert!(map.contains_key("0xc04b8d59"));
    assert!(map.contains_key("0xdb3e2198"));
    assert!(map.contains_key("0xf28c0498"));
}

#[test]
fn unknown_selector_is_not_in_map() {
    let map = known_selectors();
    assert!(!map.contains_key("0xdeadbeef"));
}

#[test]
fn decode_transfer_event_topic() {
    let topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let name = decode_event_topic(topic);
    assert!(name.contains("Transfer"), "Expected Transfer, got: {name}");
}

#[test]
fn decode_swap_v2_event_topic() {
    let topic = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";
    let name = decode_event_topic(topic);
    assert!(name.contains("Swap"), "Expected Swap, got: {name}");
    assert!(name.contains("V2"), "Expected V2 marker, got: {name}");
}

#[test]
fn decode_sync_event_topic() {
    let topic = "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1";
    let name = decode_event_topic(topic);
    assert!(name.contains("Sync"), "Expected Sync, got: {name}");
}

#[test]
fn unknown_event_topic_returns_unknown() {
    assert_eq!(decode_event_topic("0x0000000000000000000000000000000000000000000000000000000000000000"), "Unknown");
}

#[test]
fn invalid_tx_hash_returns_clear_error() {
    let result = analyze_transaction("http://localhost:1", "not-a-hash");
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("invalid") || msg.contains("hash"),
        "Error should mention invalid hash. Got: {msg}"
    );
}
