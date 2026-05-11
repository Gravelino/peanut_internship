use clap::Parser;
use rust_decimal::Decimal;

use peanut_internship_rust::core::types::{DEFAULT_ORDERBOOK_DEPTH, split_pair_symbols};
use peanut_internship_rust::exchange::{BinanceConfig, ExchangeClient, OrderBookAnalyzer};
use peanut_internship_rust::format;

const DEPTH_WINDOW_BPS: u64 = 10;
const IMBALANCE_LEVELS: usize = 10;
const IMBALANCE_PRESSURE_THRESHOLD: f64 = 0.05;
const WALK_THE_BOOK_SIZES: [u64; 2] = [2, 10];
const EFFECTIVE_SPREAD_SIZE: u64 = 2;

/// Formats a Unix-millisecond timestamp as a human-readable UTC string.
fn format_unix_millis(millis: u64) -> String {
    chrono::DateTime::from_timestamp_millis(millis as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{millis} (raw epoch ms)"))
}

#[derive(Parser)]
#[command(name = "orderbook")]
#[command(about = "Fetch and analyze Binance order books")]
struct Cli {
    symbol: String,

    #[arg(short, long, default_value_t = DEFAULT_ORDERBOOK_DEPTH)]
    depth: u32,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let config = match BinanceConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config error: {e}");
            eprintln!("Set BINANCE_TESTNET_API_KEY and BINANCE_TESTNET_SECRET in .env");
            std::process::exit(1);
        }
    };

    let client = match ExchangeClient::new(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Client init error: {e}");
            std::process::exit(1);
        }
    };

    match client.health_check().await {
        Ok(t) => println!(
            "Connected to Binance testnet (server time: {})",
            format_unix_millis(t)
        ),
        Err(e) => {
            eprintln!("Connection check failed: {e}");
            std::process::exit(1);
        }
    }

    let ob = match client.fetch_order_book(&cli.symbol, cli.depth).await {
        Ok(ob) => ob,
        Err(e) => {
            eprintln!("Failed to fetch order book: {e}");
            std::process::exit(1);
        }
    };

    let analyzer = OrderBookAnalyzer::new(ob);
    let ob = analyzer.orderbook();

    let base = match split_pair_symbols(&ob.symbol) {
        Ok((base, _)) => base,
        Err(error) => {
            eprintln!("Invalid order book symbol: {error}");
            std::process::exit(1);
        }
    };

    println!();
    println!("╔══════════════════════════════════════════════════════╗");
    println!("║  {} Order Book Analysis", pad_right(&ob.symbol, 34));
    println!("║  Timestamp: {}", format_unix_millis(ob.timestamp));
    println!("╠══════════════════════════════════════════════════════╣");

    match ob.best_bid {
        Some((p, q)) => println!(
            "║  Best Bid:    {} × {} {}",
            format::fmt_price(p),
            format::fmt_qty(q),
            base
        ),
        None => println!("║  Best Bid:    N/A"),
    }
    match ob.best_ask {
        Some((p, q)) => println!(
            "║  Best Ask:    {} × {} {}",
            format::fmt_price(p),
            format::fmt_qty(q),
            base
        ),
        None => println!("║  Best Ask:    N/A"),
    }
    match ob.mid_price {
        Some(mp) => println!("║  Mid Price:   {}", format::fmt_price(mp)),
        None => println!("║  Mid Price:   N/A"),
    }
    println!(
        "║  Spread:      {} ({})",
        match (ob.best_bid, ob.best_ask) {
            (Some((bid, _)), Some((ask, _))) => format::fmt_price(ask - bid),
            _ => "N/A".into(),
        },
        ob.spread_bps
            .map(format::fmt_bps)
            .unwrap_or_else(|| "N/A".into())
    );

    println!("╠══════════════════════════════════════════════════════╣");

    let bid_depth = analyzer
        .depth_at_bps("bid", Decimal::from(DEPTH_WINDOW_BPS))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: bid depth calculation failed: {e}");
            Decimal::ZERO
        });
    let ask_depth = analyzer
        .depth_at_bps("ask", Decimal::from(DEPTH_WINDOW_BPS))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: ask depth calculation failed: {e}");
            Decimal::ZERO
        });
    println!("║  Depth (within {DEPTH_WINDOW_BPS} bps):");
    println!("║    Bids: {} {}", format::fmt_qty(bid_depth), base);
    println!("║    Asks: {} {}", format::fmt_qty(ask_depth), base);

    let imb = analyzer.imbalance(IMBALANCE_LEVELS);
    let imb_label = if imb > IMBALANCE_PRESSURE_THRESHOLD {
        "buy pressure"
    } else if imb < -IMBALANCE_PRESSURE_THRESHOLD {
        "sell pressure"
    } else {
        "balanced"
    };
    println!("║  Imbalance: {:+.2} ({})", imb, imb_label);

    println!("╠══════════════════════════════════════════════════════╣");

    for size in WALK_THE_BOOK_SIZES.map(Decimal::from) {
        let walk = analyzer.walk_the_book("buy", size).unwrap();
        println!("║  Walk-the-book ({} {} buy):", format::fmt_qty(size), base);
        println!("║    Avg price:  {}", format::fmt_price(walk.avg_price));
        println!("║    Slippage:   {}", format::fmt_bps(walk.slippage_bps));
        println!("║    Levels:     {}", walk.levels_consumed);
        if !walk.fully_filled {
            println!("║    ⚠️  Insufficient liquidity");
        }
    }

    let eff_spread = analyzer
        .effective_spread(Decimal::from(EFFECTIVE_SPREAD_SIZE))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: effective spread calculation failed: {e}");
            Decimal::ZERO
        });
    println!("╠══════════════════════════════════════════════════════╣");
    println!(
        "║  Effective spread ({} {} round-trip): {}",
        EFFECTIVE_SPREAD_SIZE,
        base,
        format::fmt_bps(eff_spread)
    );
    println!("╚══════════════════════════════════════════════════════╝");
}

fn pad_right(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    while out.len() < width {
        out.push(' ');
    }
    out
}
