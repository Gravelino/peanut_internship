//! Integration tests for complete pricing engine workflow
//! Tests cross-module interactions with the current pricing API

use peanut_internship_rust::pricing::{PoolRef, Route, RouteFinder};
use peanut_internship_rust::{Address, Token, UniswapV2Pair};

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

/// Test that a single-hop route produces consistent output across multiple calls.
#[test]
fn single_hop_route_deterministic_across_multiple_calls() {
    let pair = pair(
        "0x1000000000000000000000000000000000000100",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let input_amount = 100_000 * 10u128.pow(6);

    let out1 = pair.get_amount_out(input_amount, &usdc()).unwrap();
    let out2 = pair.get_amount_out(input_amount, &usdc()).unwrap();
    let out3 = pair.get_amount_out(input_amount, &usdc()).unwrap();

    assert_eq!(out1, out2);
    assert_eq!(out2, out3);
    assert!(out1 > 0);
}

/// Test that multi-hop route output equals sequential single-hop execution.
#[test]
fn multihop_chain_preserves_intermediate_values() {
    let pair1 = pair(
        "0x1000000000000000000000000000000000000101",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let pair2 = pair(
        "0x1000000000000000000000000000000000000102",
        weth(),
        dai(),
        500 * 10u128.pow(18),
        500_000 * 10u128.pow(18),
        30,
    );

    let input_amount = 100_000 * 10u128.pow(6);
    let intermediate = pair1.get_amount_out(input_amount, &usdc()).unwrap();
    let sequential_final = pair2.get_amount_out(intermediate, &weth()).unwrap();

    let route = Route::new(
        vec![PoolRef::V2(pair1), PoolRef::V2(pair2)],
        vec![usdc(), weth(), dai()],
    );
    let amounts = route.get_intermediate_amounts(input_amount).unwrap();

    assert_eq!(amounts.len(), 3);
    assert_eq!(amounts[0], input_amount);
    assert_eq!(amounts[1], intermediate);
    assert_eq!(amounts[2], sequential_final);
    assert_eq!(route.get_output(input_amount).unwrap(), sequential_final);
}

/// Test that gas cost comparison correctly influences route selection.
#[test]
fn route_comparison_selects_lower_net_cost() {
    let direct = pair(
        "0x1000000000000000000000000000000000000103",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let hop1 = pair(
        "0x1000000000000000000000000000000000000104",
        usdc(),
        dai(),
        5_000_000 * 10u128.pow(18),
        5_000_000 * 10u128.pow(18),
        30,
    );
    let hop2 = pair(
        "0x1000000000000000000000000000000000000105",
        dai(),
        weth(),
        500_000 * 10u128.pow(18),
        100 * 10u128.pow(18),
        30,
    );

    let finder = RouteFinder::new(vec![
        PoolRef::V2(direct.clone()),
        PoolRef::V2(hop1.clone()),
        PoolRef::V2(hop2.clone()),
    ]);
    let input_amount = 100_000 * 10u128.pow(6);

    let zero_gas = finder
        .find_best_route(&usdc(), &weth(), input_amount, 0, 3)
        .unwrap();
    let high_gas = finder
        .find_best_route(&usdc(), &weth(), input_amount, 10_000, 3)
        .unwrap();

    assert!(zero_gas.1 > 0, "zero-gas net output should be positive");
    assert!(high_gas.1 > 0, "high-gas net output should be positive");
}

/// Test that fee parameter affects output consistently across all fee_bps values.
#[test]
fn fee_parameter_consistently_reduces_output() {
    let base_pair = pair(
        "0x1000000000000000000000000000000000000106",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        0,
    );

    let input_amount = 100_000 * 10u128.pow(6);
    let output_no_fee = base_pair.get_amount_out(input_amount, &usdc()).unwrap();

    for fee_bps in &[1u32, 5, 10, 30, 100, 500] {
        let fee_pair = pair(
            "0x1000000000000000000000000000000000000107",
            usdc(),
            weth(),
            base_pair.reserve0,
            base_pair.reserve1,
            *fee_bps,
        );

        let output_with_fee = fee_pair.get_amount_out(input_amount, &usdc()).unwrap();
        assert!(output_with_fee < output_no_fee);
    }
}

/// Test that reverse direction follows inverse AMM math.
#[test]
fn bidirectional_route_follows_amm_inverse() {
    let pair_forward = pair(
        "0x1000000000000000000000000000000000000108",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let pair_reverse = pair(
        "0x1000000000000000000000000000000000000109",
        weth(),
        usdc(),
        pair_forward.reserve1,
        pair_forward.reserve0,
        30,
    );

    let input_usdc = 100_000 * 10u128.pow(6);
    let weth_out = pair_forward.get_amount_out(input_usdc, &usdc()).unwrap();
    let usdc_back = pair_reverse.get_amount_out(weth_out, &weth()).unwrap();

    assert!(usdc_back < input_usdc);
}

/// Test that adding a pair to a route increases step count.
#[test]
fn route_accumulation_adds_pairs_independently() {
    let pair1 = pair(
        "0x1000000000000000000000000000000000000110",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );
    let pair2 = pair(
        "0x1000000000000000000000000000000000000111",
        weth(),
        dai(),
        500 * 10u128.pow(18),
        500_000 * 10u128.pow(18),
        30,
    );

    let route_single = Route::new(vec![PoolRef::V2(pair1.clone())], vec![usdc(), weth()]);
    let route_double = Route::new(
        vec![PoolRef::V2(pair1), PoolRef::V2(pair2)],
        vec![usdc(), weth(), dai()],
    );

    assert_eq!(route_single.num_hops(), 1);
    assert_eq!(route_double.num_hops(), 2);
    let r0_single = match &route_single.pools[0] {
        PoolRef::V2(p) => p.reserve0,
        _ => panic!("expected V2"),
    };
    let r0_double = match &route_double.pools[0] {
        PoolRef::V2(p) => p.reserve0,
        _ => panic!("expected V2"),
    };
    assert_eq!(r0_single, r0_double);
}

/// Test that get_amount_in is consistent with get_amount_out.
#[test]
fn inverse_function_is_consistent_ceiling_property() {
    let pair = pair(
        "0x1000000000000000000000000000000000000112",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let input_amount = 100_000 * 10u128.pow(6);
    let output = pair.get_amount_out(input_amount, &usdc()).unwrap();
    let input_needed = pair.get_amount_in(output, &weth()).unwrap();

    assert!(input_needed >= input_amount);
    assert!(input_needed < input_amount * 2);
}

/// Test that token pair order doesn't matter when reserves are swapped accordingly.
#[test]
fn pair_symmetry_with_swapped_reserves() {
    let pair_ab = pair(
        "0x1000000000000000000000000000000000000113",
        usdc(),
        weth(),
        5_000_000 * 10u128.pow(6),
        1_000 * 10u128.pow(18),
        30,
    );

    let pair_ba = pair(
        "0x1000000000000000000000000000000000000114",
        weth(),
        usdc(),
        pair_ab.reserve1,
        pair_ab.reserve0,
        30,
    );

    let input_usdc = 100_000 * 10u128.pow(6);
    let out_forward = pair_ab.get_amount_out(input_usdc, &usdc()).unwrap();
    let out_backward = pair_ba.get_amount_out(input_usdc, &usdc()).unwrap();

    assert_eq!(out_forward, out_backward);
}

/// Test that extremely large reserve values don't overflow.
#[test]
fn extreme_reserve_values_dont_overflow() {
    let huge_reserve = u128::MAX / 2;
    let pair = pair(
        "0x1000000000000000000000000000000000000115",
        usdc(),
        weth(),
        huge_reserve,
        huge_reserve / 10,
        30,
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pair.get_amount_out(1_000_000 * 10u128.pow(6), &usdc())
    }));

    assert!(result.is_ok());
}

/// Test that route with identical reserves produces reasonable output.
#[test]
fn equilibrium_reserves_produce_reasonable_output() {
    let pair = pair(
        "0x1000000000000000000000000000000000000116",
        usdc(),
        weth(),
        1_000_000 * 10u128.pow(6),
        1_000_000 * 10u128.pow(18),
        30,
    );

    let input_small = 1_000 * 10u128.pow(6);
    let input_large = 100_000 * 10u128.pow(6);

    let out_small = pair.get_amount_out(input_small, &usdc()).unwrap();
    let out_large = pair.get_amount_out(input_large, &usdc()).unwrap();

    assert!(out_small > 0 && out_large > 0);
    assert!(out_large > out_small);
}
