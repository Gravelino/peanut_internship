use ethers::types::U256;
use peanut_internship_rust::{
    Address, ArbDetector, ChainClient, MempoolMonitor, ParsedSwap, PricingEngine, Token,
    UniswapV2Pair, UniswapV3Pool, WEI_PER_GWEI,
};
use std::env;

const PAIR_WETH_USDC: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
const PAIR_WETH_USDT: &str = "0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852";
const V3_WETH_USDC_030: &str = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Arbitrage Opportunity Detector Demo ===\n");

    demo_local_arb()?;

    println!("\n--- Live mempool arb detection ---");
    let rpc_url = env::var("MAINNET_RPC_URL")
        .or_else(|_| env::var("SEPOLIA_RPC_URL"))
        .map_err(|_| "MAINNET_RPC_URL or SEPOLIA_RPC_URL is required")?;
    let ws_url =
        env::var("WS_RPC_URL").unwrap_or_else(|_| "wss://ethereum-rpc.publicnode.com".to_string());

    let client = ChainClient::new(vec![rpc_url.clone()], 3, 1)?;
    let mut engine = PricingEngine::new(client, &rpc_url, &ws_url)?;

    println!("Loading V2 pools…");
    let v2_addrs: Vec<Address> = [PAIR_WETH_USDC, PAIR_WETH_USDT]
        .iter()
        .filter_map(|&a| Address::new(a).ok())
        .collect();
    engine.load_pools(&v2_addrs).await?;
    println!("Loaded {} V2 pools", engine.pools().len());

    println!("Loading V3 pools…");
    let v3_addrs: Vec<Address> = vec![Address::new(V3_WETH_USDC_030)?];
    engine.load_v3_pools(&v3_addrs).await.unwrap_or_else(|e| {
        eprintln!("V3 load failed (continuing with V2): {e}");
    });
    println!("Loaded {} V3 pools", engine.v3_pools().len());

    let gas_gwei: u128 = env::var("GAS_PRICE_GWEI")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let detector = ArbDetector::new(
        engine.pools().values().cloned().collect(),
        engine.v3_pools().values().cloned().collect(),
        gas_gwei,
    );
    println!("ArbDetector ready (gas={gas_gwei} gwei)\n");

    println!("Connecting to mempool on {ws_url}…");
    let monitor = MempoolMonitor::new(&ws_url);
    let mut swap_rx = monitor.start().await?;
    println!("Listening for pending swaps (Ctrl+C to stop)…\n");

    while let Some(swap) = swap_rx.recv().await {
        println!(
            "SWAP tx={} {} {} in={} min_out={}",
            swap.tx_hash, swap.dex, swap.method, swap.amount_in, swap.min_amount_out,
        );

        let opportunities = detector.detect_from_swap(&swap);
        if opportunities.is_empty() {
            println!("  → no arb opportunities\n");
        } else {
            for opp in &opportunities {
                let tag = if opp.is_profitable() {
                    "PROFITABLE"
                } else {
                    "unprofitable"
                };
                println!(
                    "  → {:?} {} net={}wei gas={}wei [{tag}]",
                    opp.kind, opp.token_in, opp.net_profit_wei, opp.gas_cost_wei,
                );
            }
        }
    }

    println!("Mempool stream ended.");
    Ok(())
}

fn demo_local_arb() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Local cross-DEX arb simulation ---\n");

    let weth = Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?,
        symbol: "WETH".into(),
        decimals: 18,
    };
    let usdc = Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?,
        symbol: "USDC".into(),
        decimals: 18,
    };

    let v2_pool = UniswapV2Pair::new(
        Address::new("0x1000000000000000000000000000000000000000")?,
        weth.clone(),
        usdc.clone(),
        1_000_000_000_000_000_000_000,
        2_000_000_000_000_000_000_000,
        30,
    )?;

    let v3_pool = UniswapV3Pool::new(
        Address::new("0x2000000000000000000000000000000000000000")?,
        weth.clone(),
        usdc.clone(),
        3000,
        U256::from(79228162514264337593543950336u128),
        1_000_000_000_000_000_000,
        0,
    )?;

    let v2_out = v2_pool
        .get_amount_out(1_000_000_000_000_000_000, &weth)
        .unwrap_or(0);
    let v3_out = v3_pool
        .quote_swap(1_000_000_000_000_000_000, &weth)
        .map(|q| q.amount_out)
        .unwrap_or(0);
    println!("1 WETH → USDC: V2={v2_out}  V3={v3_out}");

    let spread = (v2_out as i128 - v3_out as i128).unsigned_abs();
    println!("Spread: {spread} raw units\n");

    let swap = ParsedSwap {
        tx_hash: "0xdemo".into(),
        router: Address::new("0x0000000000000000000000000000000000000009")?,
        dex: "UniswapV2".into(),
        method: "swapExactTokensForTokens".into(),
        token_in: Some(weth.address.clone()),
        token_out: Some(usdc.address.clone()),
        amount_in: U256::from(1_000_000_000_000_000_000u128),
        min_amount_out: U256::zero(),
        deadline: U256::from(u64::MAX),
        sender: Address::new("0x0000000000000000000000000000000000000010")?,
        gas_price: U256::from(20u128 * WEI_PER_GWEI),
    };

    let detector_0gwei = ArbDetector::new(vec![v2_pool.clone()], vec![v3_pool.clone()], 0);
    let opps = detector_0gwei.detect_from_swap(&swap);
    if opps.is_empty() {
        println!("No arb at 0 gwei (pools too similar)");
    } else {
        for opp in &opps {
            println!(
                "{:?}: net_profit={}wei gas={}wei profitable={}",
                opp.kind,
                opp.net_profit_wei,
                opp.gas_cost_wei,
                opp.is_profitable()
            );
        }
    }

    let gas_cost = 250_000u128 * 20 * WEI_PER_GWEI;
    let detector_20gwei = ArbDetector::new(vec![v2_pool], vec![v3_pool], 20);
    let opps_gas = detector_20gwei.detect_from_swap(&swap);
    if opps_gas.is_empty() {
        println!("No profitable arb at 20 gwei (gas cost = {gas_cost} wei eats profit)");
    } else {
        for opp in &opps_gas {
            println!(
                "{:?}: net_profit={}wei gas={}wei profitable={}",
                opp.kind,
                opp.net_profit_wei,
                opp.gas_cost_wei,
                opp.is_profitable()
            );
        }
    }

    Ok(())
}
