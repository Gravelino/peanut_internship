use peanut_internship_rust::core::types::{Address, Token};
use peanut_internship_rust::pricing::amm::UniswapV2Pair;
use peanut_internship_rust::pricing::router::{Route, RouteFinder};
use peanut_internship_rust::pricing::errors::PricingError;

fn mock_token(symbol: &str, addr_hex: &str) -> Token {
    Token {
        address: Address::new(addr_hex).unwrap(),
        symbol: symbol.to_string(),
        decimals: 18,
    }
}

fn setup_pools() -> (Token, Token, Token, Vec<UniswapV2Pair>) {
    let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001");
    let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002");
    let eth = mock_token("ETH", "0x0000000000000000000000000000000000000003");

    let pool_shib_usdc = UniswapV2Pair::new(
        Address::new("0x1000000000000000000000000000000000000000").unwrap(),
        shib.clone(),
        usdc.clone(),
        100_000_000_000_000_000_000, 100_000_000_000_000_000_000, // 100 ETH
        30
    ).unwrap();

    let pool_shib_eth = UniswapV2Pair::new(
        Address::new("0x2000000000000000000000000000000000000000").unwrap(),
        shib.clone(),
        eth.clone(),
        10_000_000_000_000_000_000_000, 10_000_000_000_000_000_000_000, // 10k ETH
        30
    ).unwrap();

    let pool_eth_usdc = UniswapV2Pair::new(
        Address::new("0x3000000000000000000000000000000000000000").unwrap(),
        eth.clone(),
        usdc.clone(),
        10_000_000_000_000_000_000_000, 10_000_000_000_000_000_000_000, // 10k ETH
        30
    ).unwrap();

    (shib, usdc, eth, vec![pool_shib_usdc, pool_shib_eth, pool_eth_usdc])
}

#[test]
fn test_direct_vs_multihop() {
    let (shib, usdc, _eth, pools) = setup_pools();
    let finder = RouteFinder::new(pools);

    let amount_in = 10_000_000_000_000_000_000;
    // 0 gas price so we only compare optimal gross output
    let (best_route, _net_out) = finder.find_best_route(&shib, &usdc, amount_in, 0, 3).unwrap();


    // Should choose the 2-hop SHIB -> ETH -> USDC due to good liquidity.
    assert_eq!(best_route.num_hops(), 2);
    assert_eq!(best_route.path[0].symbol, "SHIB");
    assert_eq!(best_route.path[1].symbol, "ETH");
    assert_eq!(best_route.path[2].symbol, "USDC");
}

#[test]
fn test_gas_makes_direct_better() {
    let (shib, usdc, _eth, pools) = setup_pools();
    let finder = RouteFinder::new(pools);

    let amount_in = 10_000_000_000_000_000_000; // 10 "eth"
    // At 10,000 gwei (very high), gas penalty overrides the slippage benefit.
    let gas_price_gwei = 10_000; 

    let (best_route, _net_out) = finder.find_best_route(&shib, &usdc, amount_in, gas_price_gwei, 3).unwrap();

    // Should choose 1-hop because 2-hop costs too much gas
    assert_eq!(best_route.num_hops(), 1);
    assert_eq!(best_route.path.len(), 2);
    assert_eq!(best_route.path[0].symbol, "SHIB");
    assert_eq!(best_route.path[1].symbol, "USDC");
}

#[test]
fn test_no_route_exists() {
    let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001");
    let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002");
    
    // No pools
    let finder = RouteFinder::new(vec![]);
    let res = finder.find_best_route(&shib, &usdc, 1000, 10, 3);
    
    assert!(matches!(res, Err(PricingError::NoRouteExists)));
}

#[test]
fn test_route_output_matches_sequential_swaps() {
    let (shib, usdc, eth, pools) = setup_pools();

    // 2-hop Route SHIB -> ETH -> USDC
    let multi_route = Route::new(
        vec![pools[1].clone(), pools[2].clone()], 
        vec![shib.clone(), eth.clone(), usdc.clone()]
    );
    
    let amount_in = 5000_000_000_000_000_000;
    
    // Simulated manual sequence
    let out1 = pools[1].get_amount_out(amount_in, &shib).unwrap();
    let out2 = pools[2].get_amount_out(out1, &eth).unwrap();

    // Asserts
    let route_out = multi_route.get_output(amount_in).unwrap();
    assert_eq!(route_out, out2);

    let intermediate = multi_route.get_intermediate_amounts(amount_in).unwrap();
    assert_eq!(intermediate.len(), 3);
    assert_eq!(intermediate[0], amount_in);
    assert_eq!(intermediate[1], out1);
    assert_eq!(intermediate[2], out2);
}
