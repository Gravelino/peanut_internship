use std::env;

use peanut_internship_rust::{Address, ChainClient, PricingEngine};

const PAIR_WETH_USDC: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
const PAIR_WETH_USDT: &str = "0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852";
const PAIR_USDC_USDT: &str = "0x3041CbD36888bCCa6845Fc20B539db6bF7D4e2BC";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_url = env::var("MAINNET_RPC_URL")
        .or_else(|_| env::var("SEPOLIA_RPC_URL"))
        .map_err(|_| "MAINNET_RPC_URL or SEPOLIA_RPC_URL is required")?;

    let ws_url = env::var("WS_RPC_URL").unwrap_or_else(|_| "ws://127.0.0.1:8545".to_string());

    let client = ChainClient::new(vec![rpc_url.clone()], 20, 2)?;
    let mut engine = PricingEngine::new(client, &rpc_url, &ws_url)?;

    println!("Loading pools…");
    let pool_addresses: Vec<Address> = [PAIR_WETH_USDC, PAIR_WETH_USDT, PAIR_USDC_USDT]
        .iter()
        .filter_map(|&a| Address::new(a).ok())
        .collect();

    engine.load_pools(&pool_addresses).await?;
    println!("Loaded {} pools", engine.pools().len());

    println!("Starting price feed on {ws_url}…");
    let mut rx = engine.start_price_feed(&ws_url).await?;

    println!("Listening for price ticks (Ctrl+C to stop)…\n");

    while let Some(tick) = rx.recv().await {
        println!(
            "[blk={}] {} → {} price={} reserves=({},{})",
            tick.block_number,
            tick.token_in,
            tick.token_out,
            tick.price.round_dp(6),
            tick.reserve_in,
            tick.reserve_out,
        );
    }

    println!("Price feed stream ended.");
    Ok(())
}
