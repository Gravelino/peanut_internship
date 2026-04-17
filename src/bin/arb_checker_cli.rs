use std::collections::HashMap;

use clap::Parser;
use rust_decimal::Decimal;

use peanut_internship_rust::exchange::types::NormalizedBalance;
use peanut_internship_rust::exchange::{BinanceConfig, ExchangeClient};
use peanut_internship_rust::integration::{ArbCheckResult, ArbChecker};
use peanut_internship_rust::inventory::{InventoryTracker, PnLEngine, Venue};

const WETH_USDC_V2: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
const WETH_USDT_V2: &str = "0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852";
const WBTC_ETH_V2: &str = "0xBb2b8038a16401936FCE81B262E509B2E5c43d05";

#[derive(Parser)]
#[command(name = "arb_checker")]
#[command(about = "End-to-end arbitrage opportunity checker (CEX vs DEX)")]
struct Cli {
    pair: String,

    #[arg(short, long, default_value = "2.0")]
    size: String,

    #[arg(long, default_value = "30")]
    dex_fee_bps: String,

    #[arg(long, default_value = "5.0")]
    gas_cost_usd: String,

    #[arg(long)]
    fork_url: Option<String>,

    #[arg(long)]
    pool: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let size: Decimal = Decimal::from_str_exact(&cli.size).unwrap_or(Decimal::from(2));
    let dex_fee_bps: Decimal =
        Decimal::from_str_exact(&cli.dex_fee_bps).unwrap_or(Decimal::from(30));
    let gas_cost_usd: Decimal =
        Decimal::from_str_exact(&cli.gas_cost_usd).unwrap_or(Decimal::from(5));

    let config = match BinanceConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config error: {e}");
            eprintln!("Set BINANCE_TESTNET_API_KEY and BINANCE_TESTNET_SECRET in .env");
            std::process::exit(1);
        }
    };

    let exchange_client = match ExchangeClient::new(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Client init error: {e}");
            std::process::exit(1);
        }
    };

    let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);

    match exchange_client.fetch_balance().await {
        Ok(balances) => {
            tracker.update_from_cex(Venue::Binance, balances);
        }
        Err(e) => {
            eprintln!("Warning: Could not fetch Binance balance: {e}");
            let mut demo = HashMap::new();
            demo.insert("ETH".into(), NormalizedBalance { free: Decimal::from(8), locked: Decimal::ZERO, total: Decimal::from(8) });
            demo.insert("USDT".into(), NormalizedBalance { free: Decimal::from(20000), locked: Decimal::ZERO, total: Decimal::from(20000) });
            tracker.update_from_cex(Venue::Binance, demo);
        }
    }

    let mut wallet_bals = HashMap::new();
    wallet_bals.insert("ETH".into(), Decimal::from(5));
    wallet_bals.insert("USDT".into(), Decimal::from(15000));
    tracker.update_from_wallet(Venue::Wallet, wallet_bals);

    let pnl_engine = PnLEngine::new();
    let checker = ArbChecker::new(exchange_client, tracker, pnl_engine);

    println!();
    println!("═══════════════════════════════════════════");
    println!("  ARB CHECK: {} (size: {} {})", cli.pair, size, base_asset(&cli.pair));
    println!("═══════════════════════════════════════════");
    println!();

    if let (Some(fork_url), Some(pool)) = (cli.fork_url.as_deref(), cli.pool.as_deref()) {
        println!("DEX source: Uniswap V2 via Anvil fork");
        println!("  Fork URL: {}", fork_url);
        println!("  Pool:     {}", pool);
        println!();
        println!("Fetching DEX price from Uniswap V2 pool...");

        match checker.check_with_dex(&cli.pair, size, dex_fee_bps, gas_cost_usd, fork_url, pool).await {
            Ok(result) => display_result(result),
            Err(e) => {
                eprintln!("Arb check failed: {e}");
                eprintln!();
                eprintln!("Make sure Anvil is running:");
                eprintln!("  ETH_RPC_URL=https://your-mainnet-rpc ./scripts/start_fork.sh");
                std::process::exit(1);
            }
        }
    } else if let Some(fork_url) = cli.fork_url.as_deref() {
        let pool = default_pool(&cli.pair);
        println!("DEX source: Uniswap V2 via Anvil fork");
        println!("  Fork URL: {}", fork_url);
        println!("  Pool:     {} (default for {})", pool, cli.pair);
        println!();
        println!("Fetching DEX price from Uniswap V2 pool...");

        match checker.check_with_dex(&cli.pair, size, dex_fee_bps, gas_cost_usd, fork_url, pool).await {
            Ok(result) => display_result(result),
            Err(e) => {
                eprintln!("Arb check failed: {e}");
                eprintln!();
                eprintln!("Make sure Anvil is running:");
                eprintln!("  ETH_RPC_URL=https://your-mainnet-rpc ./scripts/start_fork.sh");
                std::process::exit(1);
            }
        }
    } else {
        println!("DEX source: Price oracle (Binance, CoinGecko, Kraken, etc.)");
        println!("Tip: Use --fork-url http://127.0.0.1:8545 for real Uniswap V2 prices");
        println!();
        println!("Fetching prices from multiple sources...");

        match checker.check(&cli.pair, size, dex_fee_bps, gas_cost_usd).await {
            Ok(result) => display_result(result),
            Err(e) => {
                eprintln!("Arb check failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn display_result(result: ArbCheckResult) {
    println!();

    if let Some(pool_info) = &result.dex_pool_info {
        println!("DEX Pool (Uniswap V2):");
        println!("  Address:     {}", pool_info.pool_address);
        println!("  Pair:        {}/{}", pool_info.token0, pool_info.token1);
        println!("  Reserve0:    {}", pool_info.reserve0);
        println!("  Reserve1:    {}", pool_info.reserve1);
        println!("  Spot price:  ${:.2}", pool_info.spot_price);
        println!("  Exec price:  ${:.2}", pool_info.execution_price);
        println!("  Impact:      {} bps", pool_info.price_impact_bps.round());
        println!();
    }

    if let Some(sources) = &result.price_sources {
        println!("Price Sources ({} fetched):", sources.sources.len());
        for src in &sources.sources {
            println!("  {}:  ${}", src.name, src.price);
        }
        println!();
    }

    println!("Prices:");
    println!("  DEX price:         ${:.2} ({})", result.dex_price, result.dex_price_source);
    println!("  CEX bid:           ${:.2}", result.cex_bid);
    println!("  CEX ask:           ${:.2}", result.cex_ask);
    println!();
    println!("Gap: {} bps", result.gap_bps.round());

    println!();
    println!("Costs:");
    println!("  DEX fee:           {} bps", result.details.dex_fee_bps);
    println!("  DEX price impact:  {} bps", result.details.dex_price_impact_bps.round());
    println!("  CEX fee:           {} bps", result.details.cex_fee_bps);
    println!("  CEX slippage:      {} bps", result.details.cex_slippage_bps.round());
    println!("  Gas:               ${} (gas cost)", result.details.gas_cost_usd);
    println!("  ────────────────────────");
    println!("  Total costs:       {} bps", result.estimated_costs_bps.round());
    println!();

    let pnl_label = if result.estimated_net_pnl_bps > Decimal::ZERO { "PROFITABLE" } else { "NOT PROFITABLE" };
    let icon = if result.executable { "✅" } else { "❌" };
    let reason = if !result.inventory_ok && result.estimated_net_pnl_bps > Decimal::ZERO {
        " (profitable but insufficient inventory)"
    } else if result.inventory_ok && result.estimated_net_pnl_bps <= Decimal::ZERO {
        " (costs exceed gap)"
    } else {
        ""
    };
    println!("Net PnL estimate: {} bps {} {}{}", result.estimated_net_pnl_bps.round(), icon, pnl_label, reason);
    println!();
    println!("Inventory OK: {}", if result.inventory_ok { "✅" } else { "❌" });
    println!("Direction: {}", result.direction.as_deref().unwrap_or("N/A"));
    println!();
    println!("Verdict: {} {}", pnl_label, reason);
    println!("═══════════════════════════════════════════");
}

fn default_pool(pair: &str) -> &'static str {
    match pair {
        "ETH/USDT" => WETH_USDT_V2,
        "ETH/USDC" => WETH_USDC_V2,
        "BTC/ETH" => WBTC_ETH_V2,
        _ => WETH_USDC_V2,
    }
}

fn base_asset(symbol: &str) -> &str {
    symbol.split('/').next().unwrap_or("???")
}
