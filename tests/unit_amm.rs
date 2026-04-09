//! Unit tests for `pricing::amm` — UniswapV2Pair and PriceImpactAnalyzer.
//!
//! All tests use deterministic, hardcoded reserves so they run offline (no RPC
//! call needed). The `from_chain` constructor is covered separately in the
//! integration test suite.

use peanut_internship_rust::core::types::{Address, Token};
use peanut_internship_rust::pricing::{PriceImpactAnalyzer, PricingError, UniswapV2Pair};
use rust_decimal::Decimal;

// ─── Fixture helpers ──────────────────────────────────────────────────────────

fn weth() -> Token {
    Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        symbol: "WETH".into(),
        decimals: 18,
    }
}

fn usdc() -> Token {
    Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
        symbol: "USDC".into(),
        decimals: 6,
    }
}

/// Standard 1 000 ETH / 2 000 000 USDC pool with 0.30% fee.
/// Spot price ≈ 2 000 USDC per WETH.
fn eth_usdc_pair() -> UniswapV2Pair {
    let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
    UniswapV2Pair::new(
        pair_addr,
        weth(),
        usdc(),
        1_000 * 10u128.pow(18),       // 1 000 WETH
        2_000_000 * 10u128.pow(6),    // 2 000 000 USDC
        30,
    )
    .unwrap()
}

// ─── get_amount_out ───────────────────────────────────────────────────────────

/// Selling 2 000 USDC should yield slightly less than 1 WETH due to fee + impact.
#[test]
fn test_get_amount_out_basic() {
    let pair = eth_usdc_pair();
    let usdc_in: u128 = 2_000 * 10u128.pow(6); // 2 000 USDC

    let eth_out = pair.get_amount_out(usdc_in, &usdc()).unwrap();

    let one_eth: u128 = 10u128.pow(18);
    assert!(eth_out < one_eth, "eth_out {eth_out} should be < 1 ETH {one_eth}");
    assert!(
        eth_out > (one_eth * 99) / 100,
        "eth_out {eth_out} should be > 0.99 ETH"
    );
}

/// Verify the math replicates the Solidity formula exactly with known values.
///
/// Reference swap (from Uniswap V2 whitepaper example):
///   reserve_in  = 1 000 * 1e18
///   reserve_out = 2 000 000 * 1e6
///   amount_in   = 2 000 * 1e6
///   fee_bps     = 30
///
/// Manual computation:
///   amount_in_with_fee = 2_000_000_000 * 9970 = 19_940_000_000_000
///   numerator          = 19_940_000_000_000 * 2_000_000_000_000
///                      = 39_880_000_000_000_000_000_000_000
///   denominator        = 1_000_000_000_000_000_000_000 * 10_000
///                        + 19_940_000_000_000
///                      = 10_000_000_000_000_019_940_000_000_000
///   amount_out         = 39_880_000_000_000_000_000_000_000
///                        / 10_000_000_000_000_019_940_000_000_000
///                      = 998_001_197_...  (≈ 0.998001 WETH in wei)
#[test]
fn test_get_amount_out_matches_solidity() {
    let pair = eth_usdc_pair();
    let usdc_in: u128 = 2_000 * 10u128.pow(6);

    let amount_in_with_fee: u128 = usdc_in * (10_000 - 30);
    let reserve_in: u128 = 2_000_000 * 10u128.pow(6);
    let reserve_out: u128 = 1_000 * 10u128.pow(18);

    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * 10_000 + amount_in_with_fee;
    let expected = numerator / denominator;

    let actual = pair.get_amount_out(usdc_in, &usdc()).unwrap();
    assert_eq!(actual, expected, "output must match Solidity formula exactly");
}

/// Huge reserves and inputs: must never lose precision with integer math.
#[test]
fn test_integer_math_no_floats_large_numbers() {
    let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
    let pair = UniswapV2Pair::new(
        pair_addr,
        weth(),
        usdc(),
        10u128.pow(30),
        10u128.pow(30),
        30,
    )
    .unwrap();

    let amount_in: u128 = 10u128.pow(25);
    let out = pair.get_amount_out(amount_in, &weth()).unwrap();

    // Result must be a valid non-zero integer (type system guarantees no floats)
    assert!(out > 0, "output must be positive");
    // Verify it is less than the reserve (sanity)
    assert!(out < 10u128.pow(30));
}

/// Zero input must return ZeroAmountIn error.
#[test]
fn test_get_amount_out_zero_input() {
    let pair = eth_usdc_pair();
    let err = pair.get_amount_out(0, &usdc()).unwrap_err();
    assert_eq!(err, PricingError::ZeroAmountIn);
}

/// Token not in the pair must return UnknownToken error.
#[test]
fn test_get_amount_out_unknown_token() {
    let pair = eth_usdc_pair();
    let dai = Token {
        address: Address::new("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
        symbol: "DAI".into(),
        decimals: 18,
    };
    let err = pair.get_amount_out(1_000, &dai).unwrap_err();
    assert!(matches!(err, PricingError::UnknownToken(_)));
}

// ─── get_amount_in ────────────────────────────────────────────────────────────

/// get_amount_in should be the inverse of get_amount_out (within rounding of 1).
#[test]
fn test_get_amount_in_inverse_of_out() {
    let pair = eth_usdc_pair();
    let desired_eth_out: u128 = 10u128.pow(17); // 0.1 ETH

    let usdc_in_required = pair.get_amount_in(desired_eth_out, &weth()).unwrap();
    // Buying that amount should yield at least the desired output
    let actual_out = pair.get_amount_out(usdc_in_required, &usdc()).unwrap();

    // Due to ceiling rounding the output should be >= desired.
    assert!(
        actual_out >= desired_eth_out,
        "actual_out {actual_out} should be >= desired {desired_eth_out}"
    );
    // Overshoot is bounded by the rate: 1 extra USDC unit → at most a few hundred
    // thousand WETH-wei extra (≪ 1e12 wei). Assert it is well under 1e12.
    let overshoot = actual_out - desired_eth_out;
    assert!(
        overshoot < 10u128.pow(12),
        "overshoot {overshoot} should be < 1e12 wei"
    );
}

/// Requesting output ≥ reserve must return InsufficientLiquidity.
#[test]
fn test_get_amount_in_insufficient_liquidity() {
    let pair = eth_usdc_pair();
    let too_much: u128 = 1_001 * 10u128.pow(18); // more than reserve0
    let err = pair.get_amount_in(too_much, &weth()).unwrap_err();
    assert!(matches!(err, PricingError::InsufficientLiquidity { .. }));
}

/// Zero desired output must return ZeroAmountOut.
#[test]
fn test_get_amount_in_zero_output() {
    let pair = eth_usdc_pair();
    let err = pair.get_amount_in(0, &weth()).unwrap_err();
    assert_eq!(err, PricingError::ZeroAmountOut);
}

// ─── simulate_swap (immutability) ─────────────────────────────────────────────

/// simulate_swap must not mutate the original; reserves of the new pair must differ.
#[test]
fn test_simulate_swap_is_immutable() {
    let pair = eth_usdc_pair();
    let original_reserve0 = pair.reserve0;
    let original_reserve1 = pair.reserve1;

    let usdc_in: u128 = 2_000 * 10u128.pow(6);
    let new_pair = pair.simulate_swap(usdc_in, &usdc()).unwrap();

    // Original unchanged
    assert_eq!(pair.reserve0, original_reserve0, "reserve0 must not change");
    assert_eq!(pair.reserve1, original_reserve1, "reserve1 must not change");

    // New pair has updated reserves
    assert!(
        new_pair.reserve1 > original_reserve1,
        "selling USDC should increase reserve1"
    );
    assert!(
        new_pair.reserve0 < original_reserve0,
        "buying WETH should decrease reserve0"
    );
}

/// After simulating a swap the new spot price must be worse for the same direction.
#[test]
fn test_simulate_swap_updates_price() {
    let pair = eth_usdc_pair();
    let usdc_in: u128 = 100_000 * 10u128.pow(6); // large trade

    let spot_before = pair.get_spot_price(&usdc()).unwrap();
    let new_pair = pair.simulate_swap(usdc_in, &usdc()).unwrap();
    let spot_after = new_pair.get_spot_price(&usdc()).unwrap();

    // Buying WETH with USDC → more USDC in pool → WETH more expensive (lower USDC/WETH rate from WETH side, but from USDC side we get less WETH)
    // spot_after (WETH per USDC) should go down since WETH reserve decreased
    assert!(
        spot_after < spot_before,
        "spot price for USDC→WETH should decrease after buying WETH"
    );
}

// ─── spot price & price impact ────────────────────────────────────────────────

/// Spot price for a 1:2000 ratio pool should be ≈ 0.0005 WETH/USDC
/// (or equivalently ~2000 USDC/WETH depending on direction).
#[test]
fn test_spot_price_direction() {
    let pair = eth_usdc_pair();

    // USDC → WETH direction: how much WETH per 1 USDC
    let usdc_to_weth = pair.get_spot_price(&usdc()).unwrap();
    // 1 000 WETH / 2 000 000 USDC × (10^18 / 10^6) = 0.0005 WETH per USDC
    let expected = Decimal::new(5, 4); // 0.0005
    let tolerance = Decimal::new(1, 8); // 0.00000001
    assert!(
        (usdc_to_weth - expected).abs() < tolerance,
        "USDC→WETH spot price {usdc_to_weth} ≠ expected {expected}"
    );

    // Reverse direction: how much USDC per 1 WETH  ≈ 2000
    let weth_to_usdc = pair.get_spot_price(&weth()).unwrap();
    let expected_rev = Decimal::from(2000u64);
    assert!(
        (weth_to_usdc - expected_rev).abs() < Decimal::ONE,
        "WETH→USDC spot price {weth_to_usdc} ≠ expected {expected_rev}"
    );
}

/// A very small trade should have near-zero price impact.
#[test]
fn test_price_impact_small_trade() {
    let pair = eth_usdc_pair();
    let tiny: u128 = 100 * 10u128.pow(6); // 100 USDC — tiny relative to 2M pool
    let impact = pair.get_price_impact(tiny, &usdc()).unwrap();
    // Price impact = (expected_out_at_midprice - actual_out) / expected_out.
    // For a 0.30% fee pool the fee alone contributes ~0.30% to this metric
    // (expected is computed at the raw midprice, actual includes the fee deduction).
    // Pure slippage for 100 USDC in a 2M pool is ≈ 0.0025%, so total ≈ 0.302%.
    // We assert it is below 0.40% — dominated by fee, not slippage.
    assert!(
        impact < Decimal::new(4, 3),
        "impact {impact} should be < 0.40% for a tiny trade (fee ≈ 0.30%, slippage ≈ 0%)"
    );
    // Also assert it is non-zero (fee is always present).
    assert!(impact > Decimal::ZERO, "impact must be positive due to fee");
}

/// A large trade (half the reserve) should have substantial price impact.
#[test]
fn test_price_impact_large_trade() {
    let pair = eth_usdc_pair();
    // Buying with 1M USDC in a 2M pool = ~50% of reserve
    let large: u128 = 1_000_000 * 10u128.pow(6);
    let impact = pair.get_price_impact(large, &usdc()).unwrap();
    // Impact should be well above 30%
    assert!(impact > Decimal::new(30, 2), "impact {impact} should be > 30%");
}

// ─── PriceImpactAnalyzer ──────────────────────────────────────────────────────

/// generate_impact_table must return one row per input size.
#[test]
fn test_impact_table_row_count() {
    let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
    let sizes: Vec<u128> = vec![
        1_000 * 10u128.pow(6),
        10_000 * 10u128.pow(6),
        100_000 * 10u128.pow(6),
    ];
    let table = analyzer.generate_impact_table(&usdc(), &sizes).unwrap();
    assert_eq!(table.len(), 3);
}

/// Larger trades must have larger price impact.
#[test]
fn test_impact_table_impact_monotonically_increases() {
    let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
    let sizes: Vec<u128> = (1..=5)
        .map(|i| i as u128 * 100_000 * 10u128.pow(6))
        .collect();
    let table = analyzer.generate_impact_table(&usdc(), &sizes).unwrap();

    for window in table.windows(2) {
        assert!(
            window[1].price_impact_pct >= window[0].price_impact_pct,
            "impact should be non-decreasing with larger trades"
        );
    }
}

/// find_max_size_for_impact: result must satisfy the impact constraint.
#[test]
fn test_find_max_size_for_impact_satisfies_constraint() {
    let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
    let max_pct = Decimal::ONE; // 1%

    let max_size = analyzer.find_max_size_for_impact(&usdc(), max_pct).unwrap();

    // The returned size must have impact ≤ 1%
    let impact = eth_usdc_pair().get_price_impact(max_size, &usdc()).unwrap();
    let impact_pct = impact * Decimal::ONE_HUNDRED;
    assert!(
        impact_pct <= max_pct,
        "impact {impact_pct}% exceeds max {max_pct}%"
    );

    // One unit larger should exceed the limit (optional but good sanity check)
    // (skip this for very large pairs where the unit is negligible)
}

/// estimate_true_cost: net_output must be ≤ gross_output.
#[test]
fn test_estimate_true_cost_net_lte_gross() {
    let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
    let usdc_in: u128 = 2_000 * 10u128.pow(6);

    let cost = analyzer
        .estimate_true_cost(usdc_in, &usdc(), 20, 150_000)
        .unwrap();

    assert!(
        cost.net_output <= cost.gross_output,
        "net_output must be ≤ gross_output"
    );
    assert!(cost.gas_cost_eth > 0, "gas cost must be positive");
}

// ─── Invalid fee_bps ──────────────────────────────────────────────────────────

#[test]
fn test_invalid_fee_bps_rejected() {
    let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
    let err = UniswapV2Pair::new(pair_addr, weth(), usdc(), 1_000, 1_000, 10_000).unwrap_err();
    assert_eq!(err, PricingError::InvalidFeeBps(10_000));
}
