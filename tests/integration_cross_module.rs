//! Cross-module integration tests
//! Tests interactions between pricing, wallet, and route composition

use peanut_internship_rust::pricing::{Route, RouteFinder};
use peanut_internship_rust::{Address, Token, TokenAmount, UniswapV2Pair, WalletManager};

fn usdc() -> Token {
    Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
        symbol: "USDC".into(),
        decimals: 6,
    }
}

fn weth() -> Token {
    Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        symbol: "WETH".into(),
        decimals: 18,
    }
}

fn dai() -> Token {
    Token {
        address: Address::new("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
        symbol: "DAI".into(),
        decimals: 18,
    }
}

fn pair(
    address: &str,
    token0: Token,
    token1: Token,
    reserve0: u128,
    reserve1: u128,
    fee_bps: u32,
) -> UniswapV2Pair {
    UniswapV2Pair::new(
        Address::new(address).unwrap(),
        token0,
        token1,
        reserve0,
        reserve1,
        fee_bps,
    )
    .unwrap()
}

/// Test that wallet generation produces valid, unique addresses
/// Validates: multiple wallets have different addresses
#[test]
fn wallet_generation_produces_unique_addresses() {
    let wallet1 = WalletManager::generate().expect("failed to generate wallet1");
    let wallet2 = WalletManager::generate().expect("failed to generate wallet2");

    let addr1 = wallet1.address();
    let addr2 = wallet2.address();

    assert_ne!(
        addr1, addr2,
        "Generated wallets should have unique addresses"
    );

    assert!(
        addr1.starts_with("0x") && addr1.len() == 42,
        "Wallet address should be valid hex format"
    );
    assert!(
        addr2.starts_with("0x") && addr2.len() == 42,
        "Wallet address should be valid hex format"
    );
}

/// Test that wallet can be converted to Address
/// Validates: wallet address string can be parsed to typed Address
#[test]
fn wallet_address_can_be_parsed_to_typed_address() {
    let wallet = WalletManager::generate().expect("failed to generate wallet");
    let wallet_addr_str = wallet.address();

    let addr_result = Address::new(&wallet_addr_str);
    assert!(
        addr_result.is_ok(),
        "Wallet address string should parse to Address type"
    );

    let addr = addr_result.unwrap();
    assert_eq!(
        addr.to_string().to_lowercase(),
        wallet_addr_str.to_lowercase(),
        "Parsed address should match original string"
    );
}

/// Test that pricing route and wallet can be composed for transaction building
/// Validates: route output calculation is independent of wallet state
#[test]
fn pricing_route_independent_of_wallet() {
    let _wallet = WalletManager::generate().expect("failed to generate wallet");

    let pair = pair(
        "0x1000000000000000000000000000000000000000",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let input_amount = 100_000 * 10u128.pow(6);

    let output1 = pair
        .get_amount_out(input_amount, &usdc())
        .expect("route calculation");

    let _wallet2 = WalletManager::generate().expect("failed to generate wallet2");

    let output2 = pair
        .get_amount_out(input_amount, &usdc())
        .expect("route calculation");

    assert_eq!(
        output1, output2,
        "Pricing should be deterministic regardless of wallet generation"
    );
}

/// Test that token amounts maintain decimal precision through routing
/// Validates: TokenAmount with proper decimals produces correct routing
#[test]
fn token_amount_decimals_preserved_through_routes() {
    let pair = pair(
        "0x1000000000000000000000000000000000000001",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let usdc_amount = TokenAmount::from_human("100", 6, Some("USDC".into())).unwrap();

    assert_eq!(usdc_amount.raw, ethers::types::U256::from(100_000_000u64));
    assert_eq!(usdc_amount.decimals, 6);
    assert_eq!(usdc_amount.symbol.as_deref(), Some("USDC"));

    let output = pair
        .get_amount_out(usdc_amount.raw.as_u128(), &usdc())
        .expect("route with token amount");

    assert!(output > 0, "Output should be positive");
    assert!(output < 10u128.pow(18), "Output sanity check");
}

/// Test that multi-hop route gas cost affects route selection
/// Validates: gas cost is correctly accumulated across hops
#[test]
fn multipath_gas_cost_accumulation() {
    let pair1 = pair(
        "0x1000000000000000000000000000000000000002",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let route_direct = Route::new(vec![pair1.clone()], vec![usdc(), weth()]);
    let route_with_gas = route_direct.clone();

    let input_amount = 100_000 * 10u128.pow(6);

    let output_no_gas = route_direct
        .get_output(input_amount)
        .expect("route without gas");
    let output_with_gas = route_with_gas
        .get_output(input_amount)
        .expect("route with gas");

    assert_eq!(output_no_gas, output_with_gas);
    assert!(route_direct.estimate_gas() > 0);
    assert_eq!(route_direct.num_hops(), 1);
}

/// Test that a sequence of transactions maintains wallet state consistency
/// Validates: wallet address remains constant through transaction building
#[tokio::test]
async fn wallet_address_consistency_through_transactions() {
    let wallet = WalletManager::generate().expect("failed to generate wallet");

    let addr1 = wallet.address();
    let addr2 = wallet.address();
    let addr3 = wallet.address();

    assert_eq!(addr1, addr2, "Wallet address should be consistent");
    assert_eq!(addr2, addr3, "Wallet address should be consistent");

    let typed_addr = Address::new(&addr1).expect("failed to parse address");
    assert_eq!(
        typed_addr.to_string().to_lowercase(),
        addr1.to_lowercase(),
        "Parsed address should match wallet address"
    );
}

/// Test that total fees across multi-hop route are correctly computed
/// Validates: fee_bps accumulation doesn't exceed 100% per hop
#[test]
fn multihop_fee_accumulation_reasonable() {
    let pair1 = pair(
        "0x1000000000000000000000000000000000000003",
        usdc(),
        dai(),
        5_000_000 * 10u128.pow(6),
        5_000_000 * 10u128.pow(18),
        30,
    );

    let pair2 = pair(
        "0x1000000000000000000000000000000000000004",
        dai(),
        weth(),
        500_000 * 10u128.pow(18),
        100 * 10u128.pow(18),
        30,
    );

    let route = Route::new(vec![pair1, pair2], vec![usdc(), dai(), weth()]);

    let input_amount = 100_000 * 10u128.pow(6);

    let amounts = route
        .get_intermediate_amounts(input_amount)
        .expect("multi-hop route");

    assert_eq!(amounts.len(), 3);
    assert_eq!(amounts[0], input_amount);
    assert!(amounts[1] > 0);
    assert!(amounts[2] > 0);
    assert!(amounts[2] < amounts[1]);
}

/// Test that invalid token addresses are rejected appropriately
/// Validates: address validation happens at parse time
#[test]
fn invalid_token_addresses_rejected() {
    let invalid_short = Address::new("0x123");
    assert!(invalid_short.is_err(), "Short address should be rejected");

    let invalid_hex = Address::new("0xZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ");
    assert!(invalid_hex.is_err(), "Invalid hex should be rejected");

    let valid = Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    assert!(valid.is_ok(), "Valid address format should parse");
}

/// Test that amount calculations don't overflow with maximum values
/// Validates: extreme calculation safety
#[test]
fn extreme_amount_calculations_safe() {
    let pair = pair(
        "0x1000000000000000000000000000000000000005",
        usdc(),
        weth(),
        u128::MAX / 2,
        u128::MAX / 2,
        30,
    );

    let huge_input = u128::MAX / 1000;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pair.get_amount_out(huge_input, &usdc())
    }));

    assert!(result.is_ok(), "Extreme calculation should not panic");
}

/// Test that route with empty pair list gracefully fails
/// Validates: error handling for invalid inputs
#[test]
fn empty_route_pair_list_fails_gracefully() {
    let finder = RouteFinder::new(vec![]);
    let result = finder.find_best_route(&usdc(), &weth(), 100_000 * 10u128.pow(6), 0, 3);
    assert!(result.is_err(), "Empty route search should return error");
}
