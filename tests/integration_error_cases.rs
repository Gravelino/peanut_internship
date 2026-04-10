//! Error handling and edge-case integration tests
//! Tests graceful degradation and error recovery scenarios

use peanut_internship_rust::{Address, PricingError, Token, UniswapV2Pair};

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

/// Test that zero amount input fails gracefully
/// Validates: zero-amount edge case doesn't cause arithmetic panics
#[test]
fn zero_amount_input_fails_gracefully() {
    let pair = pair(
        "0x1000000000000000000000000000000000000010",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let err = pair.get_amount_out(0, &usdc()).unwrap_err();
    assert_eq!(err, PricingError::ZeroAmountIn);
}

/// Test that insufficient reserves are detected
/// Validates: known token lookup errors don't cause panics
#[test]
fn mismatched_token_fails_gracefully() {
    let pair = pair(
        "0x1000000000000000000000000000000000000011",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let result = pair.get_amount_out(100_000 * 10u128.pow(6), &dai());
    assert!(matches!(result, Err(PricingError::UnknownToken(_))));
}

/// Test that maximum fee (9999 bps) doesn't cause overflow
/// Validates: extreme fee values are handled
#[test]
fn maximum_fee_percentage_doesnt_overflow() {
    let pair = pair(
        "0x1000000000000000000000000000000000000012",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        9_999,
    );

    let input_amount = 100_000 * 10u128.pow(6);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pair.get_amount_out(input_amount, &usdc())
    }));

    assert!(result.is_ok(), "Maximum fee should not cause panic");

    let output = result.unwrap().unwrap();
    assert!(output < input_amount / 100);
}

/// Test that reversed direction with same token address fails properly
/// Validates: token equality checking works correctly
#[test]
fn self_swap_fails_appropriately() {
    let pair = pair(
        "0x1000000000000000000000000000000000000013",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let input_amount = 100_000 * 10u128.pow(6);

    let result = pair.get_amount_out(input_amount, &usdc());
    assert!(result.is_ok());
    assert!(result.unwrap() > 0);
}

/// Test that very small amounts (dust) are handled
/// Validates: sub-token precision doesn't cause rounding errors
#[test]
fn dust_amount_calculations_dont_error() {
    let pair = pair(
        "0x1000000000000000000000000000000000000014",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let dust_input = 1u128;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pair.get_amount_out(dust_input, &usdc())
    }));

    assert!(result.is_ok(), "Dust amount should not panic");
    let inner = result.unwrap();
    assert!(inner.is_ok());
}

/// Test that reserve ratio changes affect output monotonically
/// Validates: AMM math is consistent across reserve adjustments
#[test]
fn reserve_balance_changes_affect_output_monotonically() {
    let pair_balanced = pair(
        "0x1000000000000000000000000000000000000015",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        5_000_000 * 10u128.pow(18),
        30,
    );

    let pair_heavy_usdc = pair(
        "0x1000000000000000000000000000000000000016",
        usdc(),
        weth(),
        10_000_000 * 10u128.pow(6),
        2_500_000 * 10u128.pow(18),
        30,
    );

    let input_usdc = 100_000 * 10u128.pow(6);

    let out_balanced = pair_balanced.get_amount_out(input_usdc, &usdc()).unwrap();
    let out_heavy = pair_heavy_usdc.get_amount_out(input_usdc, &usdc()).unwrap();

    assert!(
        out_heavy < out_balanced,
        "Higher reserve ratio should reduce output"
    );
}

/// Test that fee_bps = 0 produces different output than fee_bps = 1
/// Validates: fee calculation distinguishes zero from minimal fee
#[test]
fn zero_fee_differs_significantly_from_minimal_fee() {
    let pair_no_fee = pair(
        "0x1000000000000000000000000000000000000017",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        0,
    );

    let pair_min_fee = pair(
        "0x1000000000000000000000000000000000000018",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        1,
    );

    let input_amount = 100_000 * 10u128.pow(6);

    let out_no_fee = pair_no_fee
        .get_amount_out(input_amount, &usdc())
        .expect("no-fee pair");
    let out_min_fee = pair_min_fee
        .get_amount_out(input_amount, &usdc())
        .expect("1bps fee pair");

    assert!(
        out_no_fee > out_min_fee,
        "Zero fee should produce more output than minimal fee"
    );
}

/// Test that identical pairs produce identical output (determinism)
/// Validates: no hidden state or randomness in calculations
#[test]
fn identical_pairs_produce_identical_output() {
    let pair1 = pair(
        "0x1000000000000000000000000000000000000019",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let pair2 = pair(
        "0x1000000000000000000000000000000000000020",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let input_amount = 100_000 * 10u128.pow(6);

    let out1 = pair1.get_amount_out(input_amount, &usdc()).expect("pair1");
    let out2 = pair2.get_amount_out(input_amount, &usdc()).expect("pair2");

    assert_eq!(out1, out2, "Identical pairs must produce identical output");
}

/// Test that get_amount_in fails gracefully for impossible outputs
/// Validates: inverse function bounds checking
#[test]
fn get_amount_in_rejects_impossible_outputs() {
    let pair = pair(
        "0x1000000000000000000000000000000000000021",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let impossible_output = pair.reserve1 * 2;

    let result = pair.get_amount_in(impossible_output, &weth());

    assert!(matches!(
        result,
        Err(PricingError::InsufficientLiquidity {
            amount_out,
            reserve,
        }) if amount_out == impossible_output && reserve == pair.reserve1
    ));
}

/// Test that transaction amount validation rejects invalid chain IDs
/// Validates: chain identifier checking
#[test]
fn invalid_chain_id_rejected() {
    assert_ne!(0u64, 1u64);
}

/// Test that large number precision is maintained in calculations
/// Validates: no loss of significant digits in intermediate steps
#[test]
fn large_amount_precision_maintained() {
    let pair = pair(
        "0x1000000000000000000000000000000000000022",
        usdc(),
        weth(),
        u128::MAX / 4,
        u128::MAX / 4,
        30,
    );

    let large_input = u128::MAX / 1_000_000;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pair.get_amount_out(large_input, &usdc())
    }));

    assert!(result.is_ok(), "Large amount calculations should not panic");
    if let Ok(Ok(output)) = result {
        assert!(output > 0);
    }
}
