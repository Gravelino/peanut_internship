use std::env;

use dotenvy::dotenv;
use ethers::abi::{ParamType, Token as AbiToken};
use ethers::types::{Bytes, H160, U256};
use peanut_internship_rust::pricing::{MempoolMonitor, PriceImpactAnalyzer, RouteFinder};
use peanut_internship_rust::{
    Address, BlockId, ChainClient, MAINNET_CHAIN_ID, Token, TokenAmount, TransactionRequest,
    UniswapV2Pair,
};

const MAINNET_WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const MAINNET_USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const MAINNET_USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";

const PAIR_WETH_USDC: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
const PAIR_WETH_USDT: &str = "0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852";
const PAIR_USDC_USDT: &str = "0x3041CbD36888bECc7bbCBc0045E3B1f144466f5f";

const UNISWAP_V2_ROUTER: &str = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D";
const GET_AMOUNTS_OUT_SELECTOR: &str = "d06ca61f";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenv();

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
        "impact-table" => run_impact_table().await?,
        "best-route" => run_best_route().await?,
        "mempool" => run_mempool().await?,
        "solidity-check" => run_solidity_check().await?,
        _ => print_usage(),
    }

    Ok(())
}

async fn run_impact_table() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = env::var("MAINNET_RPC_URL")
        .map_err(|_| "MAINNET_RPC_URL is required for impact-table demo")?;

    let client = ChainClient::new(vec![rpc], 20, 1);
    let pair_addr = Address::new(PAIR_WETH_USDC)?;
    let pair = UniswapV2Pair::from_chain(pair_addr, &client).await?;

    let token_in = pair
        .token0_if_matches(MAINNET_USDC)
        .unwrap_or_else(|| pair.token1.clone());

    let analyzer = PriceImpactAnalyzer::new(pair.clone());
    let sizes = vec![
        1_000u128 * 10u128.pow(token_in.decimals as u32),
        10_000u128 * 10u128.pow(token_in.decimals as u32),
        100_000u128 * 10u128.pow(token_in.decimals as u32),
        500_000u128 * 10u128.pow(token_in.decimals as u32),
    ];

    let rows = analyzer.generate_impact_table(&token_in, &sizes)?;

    println!("Pool: {}", pair.address);
    println!(
        "Pair: {}/{} | reserves: {} / {}",
        pair.token0.symbol, pair.token1.symbol, pair.reserve0, pair.reserve1
    );
    println!("Token in: {}", token_in.symbol);
    println!("amount_in_raw, amount_out_raw, impact_percent");
    for row in rows {
        println!(
            "{}, {}, {:.6}",
            row.amount_in, row.amount_out, row.price_impact_pct
        );
    }

    Ok(())
}

async fn run_best_route() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = env::var("MAINNET_RPC_URL")
        .map_err(|_| "MAINNET_RPC_URL is required for best-route demo")?;

    let client = ChainClient::new(vec![rpc], 20, 1);

    let pool_addrs = [PAIR_WETH_USDC, PAIR_WETH_USDT, PAIR_USDC_USDT]
        .iter()
        .map(|a| Address::new(*a))
        .collect::<Result<Vec<_>, _>>()?;

    let mut pools = Vec::with_capacity(pool_addrs.len());
    for addr in pool_addrs {
        pools.push(UniswapV2Pair::from_chain(addr, &client).await?);
    }

    let finder = RouteFinder::new(pools);

    let token_in = Token {
        address: Address::new(MAINNET_WETH)?,
        symbol: "WETH".to_string(),
        decimals: 18,
    };
    let token_out = Token {
        address: Address::new(MAINNET_USDT)?,
        symbol: "USDT".to_string(),
        decimals: 6,
    };

    let amount_in = 1_000_000_000_000_000_000u128;
    let gas_price_gwei = 25u128;

    let (best, net_out) =
        finder.find_best_route(&token_in, &token_out, amount_in, gas_price_gwei, 3)?;

    println!("Input: {} {} (raw {})", 1, token_in.symbol, amount_in);
    println!("Gas price: {} gwei", gas_price_gwei);
    println!("Best route hops: {}", best.num_hops());
    let path_symbols = best
        .path
        .iter()
        .map(|t| t.symbol.as_str())
        .collect::<Vec<_>>()
        .join(" -> ");
    println!("Path: {}", path_symbols);

    let comparisons = finder.compare_routes(&token_in, &token_out, amount_in, gas_price_gwei, 3);
    println!("--- route comparisons ---");
    for c in comparisons {
        let symbols = c
            .route
            .path
            .iter()
            .map(|t| t.symbol.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        println!(
            "{} | gross={} net={} gas_estimate={} gas_cost_wei={}",
            symbols, c.gross_output, c.net_output, c.gas_estimate, c.gas_cost_eth
        );
    }

    println!("Best net output raw: {}", net_out);
    Ok(())
}

async fn run_mempool() -> Result<(), Box<dyn std::error::Error>> {
    let ws = env::var("WS_RPC_URL").unwrap_or_else(|_| "ws://127.0.0.1:8545".to_string());
    let monitor = MempoolMonitor::new(ws.clone());

    println!("Starting mempool monitor on {}", ws);
    println!("Waiting for swap transactions... press Ctrl+C to stop");

    let mut rx = monitor.start().await?;
    let mut seen = 0u64;

    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await {
            Ok(Some(swap)) => {
                seen += 1;
                let token_in = swap
                    .token_in
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".to_string());
                let token_out = swap
                    .token_out
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".to_string());

                println!(
                    "[{}] swap tx={} dex={} method={} token_in={} token_out={} amount_in={} min_out={} gas_price={}",
                    seen,
                    swap.tx_hash,
                    swap.dex,
                    swap.method,
                    token_in,
                    token_out,
                    swap.amount_in,
                    swap.min_amount_out,
                    swap.gas_price,
                );
            }
            Ok(None) => {
                println!("mempool channel closed");
                break;
            }
            Err(_) => {
                println!("still waiting for pending swap tx... seen={}", seen);
            }
        }
    }

    Ok(())
}

async fn run_solidity_check() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = env::var("MAINNET_RPC_URL")
        .map_err(|_| "MAINNET_RPC_URL is required for solidity-check demo")?;

    let client = ChainClient::new(vec![rpc], 20, 1);

    let pair_address = Address::new(PAIR_WETH_USDC)?;
    let pair = UniswapV2Pair::from_chain(pair_address, &client).await?;

    let usdc = Token {
        address: Address::new(MAINNET_USDC)?,
        symbol: "USDC".to_string(),
        decimals: 6,
    };
    let weth = Token {
        address: Address::new(MAINNET_WETH)?,
        symbol: "WETH".to_string(),
        decimals: 18,
    };

    let amount_in: u128 = 2_000 * 10u128.pow(6);
    let local_out = pair.get_amount_out(amount_in, &usdc)?;

    let router = Address::new(UNISWAP_V2_ROUTER)?;
    let selector = hex::decode(GET_AMOUNTS_OUT_SELECTOR)?;
    let path = vec![
        AbiToken::Address(H160::from_slice(usdc.address.as_eth_address().as_bytes())),
        AbiToken::Address(H160::from_slice(weth.address.as_eth_address().as_bytes())),
    ];

    let mut calldata = selector;
    calldata.extend_from_slice(&ethers::abi::encode(&[
        AbiToken::Uint(U256::from(amount_in)),
        AbiToken::Array(path),
    ]));

    let req = TransactionRequest {
        to: router,
        value: TokenAmount::eth(0u64),
        data: Bytes::from(calldata),
        nonce: None,
        gas_limit: None,
        max_fee_per_gas: None,
        max_priority_fee: None,
        chain_id: MAINNET_CHAIN_ID,
    };

    let raw = client.call(&req, BlockId::Latest).await?;

    let decoded = ethers::abi::decode(&[ParamType::Array(Box::new(ParamType::Uint(256)))], &raw)?;
    let amounts = decoded[0]
        .clone()
        .into_array()
        .ok_or("router decode failed")?;
    let solidity_out = amounts
        .last()
        .and_then(|t| t.clone().into_uint())
        .ok_or("router output missing")?
        .as_u128();

    println!("Local AMM out (raw): {}", local_out);
    println!("Router getAmountsOut (raw): {}", solidity_out);
    println!("Match: {}", local_out == solidity_out);

    Ok(())
}

fn print_usage() {
    println!("pricing_demo usage:");
    println!("  cargo run --bin pricing_demo -- impact-table");
    println!("  cargo run --bin pricing_demo -- best-route");
    println!("  cargo run --bin pricing_demo -- mempool");
    println!("  cargo run --bin pricing_demo -- solidity-check");
    println!();
    println!("Required env vars:");
    println!("  MAINNET_RPC_URL for impact-table, best-route, solidity-check");
    println!("  WS_RPC_URL (optional) for mempool, default ws://127.0.0.1:8545");
}

trait TokenMatcher {
    fn token0_if_matches(&self, address: &str) -> Option<Token>;
}

impl TokenMatcher for UniswapV2Pair {
    fn token0_if_matches(&self, address: &str) -> Option<Token> {
        let target = Address::new(address).ok()?;
        if self.token0.address == target {
            Some(self.token0.clone())
        } else if self.token1.address == target {
            Some(self.token1.clone())
        } else {
            None
        }
    }
}
