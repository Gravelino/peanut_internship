use peanut_internship_rust::{
    Address, ChainClient, PoolRef, PricingEngine, RouteFinder, Token, UniswapV2Pair, UniswapV3Pool,
};
use ethers::types::U256;

const V3_WETH_USDC_030: &str = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8";
const RPC_URL: &str = "https://eth.llamarpc.com";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Uniswap V3 Concentrated Liquidity Demo ===\n");

    demo_local_math()?;

    println!("\n--- On-chain pool loading ---");
    let client = ChainClient::new(vec![RPC_URL.to_string()], 3, 1)?;
    let ws = "wss://ethereum-rpc.publicnode.com";
    let mut engine = PricingEngine::new(client, RPC_URL, ws)?;

    let addr = Address::new(V3_WETH_USDC_030)?;
    match engine.load_v3_pools(std::slice::from_ref(&addr)).await {
        Ok(()) => {
            let pool = &engine.v3_pools()[&addr];
            println!("Loaded pool: tick={}, liq={}, fee={}bps",
                pool.tick, pool.liquidity, pool.fee_bps);
            if let Ok(price) = pool.get_spot_price(&pool.token0) {
                println!("Spot price (token0→1): {price}");
            }
        }
        Err(e) => eprintln!("Failed to load on-chain pool: {e}"),
    }

    Ok(())
}

fn demo_local_math() -> Result<(), Box<dyn std::error::Error>> {
    let weth = Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?,
        symbol: "WETH".into(),
        decimals: 18,
    };
    let usdc = Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?,
        symbol: "USDC".into(),
        decimals: 6,
    };

    println!("--- V3 TickMath + swap quotes ---\n");
    for tick in [-200, -100, 0, 100, 200] {
        let pool = UniswapV3Pool::new(
            Address::new("0x0000000000000000000000000000000000000001")?,
            weth.clone(),
            usdc.clone(),
            3000,
            U256::from(79228162514264337593543950336u128),
            1_000_000_000_000_000_000,
            tick,
        )?;

        let price = pool.get_spot_price(&weth)?;
        let quote = pool.quote_swap(1_000_000_000_000_000_000, &weth)?;
        println!("tick={tick:>5}  price={price}  quote_out={} gas={} partial={}",
            quote.amount_out, quote.gas_estimate, quote.is_partial);
    }

    println!("\n--- Mixed V2+V3 route finding ---\n");
    let shib = Token { address: Address::new("0x0000000000000000000000000000000000000001")?, symbol: "SHIB".into(), decimals: 18 };
    let eth = Token { address: Address::new("0x0000000000000000000000000000000000000002")?, symbol: "ETH".into(), decimals: 18 };
    let usdc_t = Token { address: Address::new("0x0000000000000000000000000000000000000003")?, symbol: "USDC".into(), decimals: 6 };

    let v2 = UniswapV2Pair::new(
        Address::new("0x1000000000000000000000000000000000000000")?,
        shib.clone(), eth.clone(),
        10_000_000_000_000_000_000_000,
        10_000_000_000_000_000_000_000, 30,
    )?;

    let v3 = UniswapV3Pool::new(
        Address::new("0x2000000000000000000000000000000000000000")?,
        eth.clone(), usdc_t.clone(), 3000,
        U256::from(79228162514264337593543950336u128),
        1_000_000_000_000_000_000, 0,
    )?;

    let finder = RouteFinder::new(vec![PoolRef::V2(v2), PoolRef::V3(v3)]);
    match finder.find_best_route(&shib, &usdc_t, 1_000_000_000_000_000_000, 0, 3) {
        Ok((route, net)) => {
            println!("Best route: {} hops, net_out={}", route.num_hops(), net);
            for (i, pr) in route.pools.iter().enumerate() {
                println!("  hop {}: {} pool {}", i + 1,
                    if pr.is_v3() { "V3" } else { "V2" }, pr.address());
            }
        }
        Err(e) => println!("No route: {e}"),
    }

    Ok(())
}
